---
title: "Text analysis"
description: "How Persia DB tokenizes full-text search in Rust: stemming, stopwords, and the built-in analyzers for English, Spanish, Portuguese, French, German, and Italian."
head:
  - tag: title
    content: "Full-text search tokenizer, stemming, and stopwords in Rust | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Full-text search tokenizer, stemming, and stopwords in Rust | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Full-text search tokenizer, stemming, and stopwords in Rust | PersiaDB"
  - tag: script
    attrs:
      type: application/ld+json
    content: |
      {
        "@context": "https://schema.org",
        "@type": "FAQPage",
        "name": "Text analysis",
        "url": "https://tomiok.github.io/persiaDB/guide/text-analysis/",
        "mainEntity": [
          {
            "@type": "Question",
            "name": "Does Persia DB tokenize and stem full-text search in Rust today?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. Each text field names an analyzer in its collection's schema, and the same analyzer, at the same version, analyzes the documents put into it and the words of a search. Nine built-in analyzers ship, each at version 1."
            }
          },
          {
            "@type": "Question",
            "name": "Which analyzers are built in?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "standard, keyword, whitespace, en, es, pt, fr, de, and it, each at version 1. standard is Unicode word boundaries, NFKC, and lowercase, with no stopwords and no stemmer. keyword keeps the whole value as one token. whitespace splits on Unicode whitespace and keeps case and punctuation. en, es, pt, fr, de, and it add that language's stopword list and Snowball stemmer, then accent folding. es is the one that keeps ñ."
            }
          },
          {
            "@type": "Question",
            "name": "Do keyword and whitespace analyzers stem or lowercase?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "No. Each one's only token filter is the length guard, which truncates a token past 255 bytes. keyword emits the whole value, spaces included, and emits nothing for an empty value. whitespace splits on Unicode whitespace and leaves punctuation in the token. Case stays as written in both."
            }
          },
          {
            "@type": "Question",
            "name": "Does query text use the same analyzer as the indexed field?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes for every built-in analyzer. Query analysis skips only filters that stack tokens. The only such filter is edge_ngram, and no built-in analyzer includes it, so index-time and query-time tokens match."
            }
          },
          {
            "@type": "Question",
            "name": "Can I call Db::open and choose an analyzer?",
            "acceptedAnswer": {
              "@type": "Answer",
              "text": "Yes. A text field names a built-in analyzer in the collection definition, as en (its current version) or en@1 (pinned), or in code as TextOptions::analyzer with an AnalyzerRef. The embedded API has no way to register a custom analyzer yet."
            }
          }
        ]
      }
---

> **Status: pre-alpha.** %%PRE_ALPHA%%
> Counts below are read from [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) when the site is built:
> %%PROGRESS_SUMMARY%%.
> When this page and [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) disagree, the SPEC wins.
> Nothing here is a benchmark.

An analyzer turns field text into the terms the index stores ([SPEC §6.3](https://github.com/tomiok/persiaDB/blob/main/SPEC.md#63-analyzers)). The same pipeline, at the same version, turns the words of a search into those terms. Stemming and stopwords live in that pipeline. They are not a separate query option.

A `text` field names its analyzer in the collection's schema (`Db::create_collection_json`, or `TextOptions::analyzer` with `SchemaBuilder`). `Collection::put` analyzes the document's text, and `Collection::search` analyzes `q` with the analyzer of each field it searches. The embedded API does not hand out an analyzer to call on its own text; what a field indexed shows in an explained hit (`FieldExplanation::term`), as the sample below does.

The sample opens a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{Db, Explanation, FieldKind, RelaxOptions, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("docs.persia"))?;
    let docs = db.create_collection_json(
        r#"{"name": "docs", "fields": {
            "title": {"type": "text", "analyzer": "en", "positions": true},
            "sku": {"type": "keyword"}
        }}"#,
    )?;
    docs.put_json(r#"{"id": "d1", "title": "John’s cars", "sku": "AB-12"}"#)?;
    docs.put_json(r#"{"id": "d2", "title": "running shoes", "sku": "CD-34"}"#)?;

    // Relaxation off, so each search shows what the analyzer alone matches.
    let exact = |q: &str| SearchRequest::new(q).relax(RelaxOptions::disabled()).explain(true);
    let ids = |q: &str| -> Result<Vec<String>, persia::Error> {
        let page = docs.search(&exact(q))?;
        Ok(page.hits.iter().map(|h| h.document.id().to_owned()).collect())
    };

    // `en` stems both sides: "cars" was indexed as "car", "running" as "run".
    assert_eq!(ids("car")?, ["d1"]);
    assert_eq!(ids("run")?, ["d2"]);
    // "the" is a stopword: the query requires "car" alone.
    assert_eq!(ids("the cars")?, ["d1"]);

    // An explained hit names the term the index holds.
    let page = docs.search(&exact("running"))?;
    let tree = page.hits[0].explanation.as_ref().ok_or("no explanation")?;
    let Explanation::Word(word) = tree.scoring() else { return Err("not a word".into()) };
    assert_eq!(word.fields[0].term, b"run");

    // A keyword field is not analyzed: the value as written.
    assert_eq!(ids("sku:AB-12")?, ["d1"]);
    assert!(ids("sku:ab-12")?.is_empty());

    // The schema stores the analyzer with its version.
    let schema = docs.schema()?;
    let title = schema.field("title").ok_or("no title field")?;
    let FieldKind::Text(options) = title.kind() else { return Err("not a text field".into()) };
    assert_eq!(options.analyzer.to_string(), "en@1");

    db.close()?;
    Ok(())
}
```

## What an analyzer is

The pipeline is character filters, then one tokenizer, then token filters (SPEC §6.3). A token is its text, a byte span of the original value, and a position. The span is on UTF-8 character boundaries. Filters may rewrite the text. They do not move that span. The position counts from 0 inside one value. A filter that removes a token leaves a gap, so a phrase does not match across it.

No built-in analyzer has a character filter. A character filter that rewrites the characters maps spans back onto the original value (SPEC §6.3.1). The chains below start at the tokenizer.

These filters are what the built-in chains actually run, in this order when they run:

| Filter | What it does |
|---|---|
| `nfkc` | Unicode NFKC on the token text (`ﬁ` → `fi`). The token is not split again. |
| `lowercase` | Unicode default lowercase, not case folding, and not Turkish or Azeri casing. `ß` stays `ß`. `ẞ` becomes `ß`. |
| `apostrophe` | `‘` and `’` become `'`. The English stemmer only treats `'` as an apostrophe. |
| `elision` | Drops a leading French or Italian elided word and the `'` after it, when more letters follow (`l'été` → `été`). |
| `stop` | Drops a token whose text is exactly a word on that language's list. |
| `stem` | Replaces the text with the Snowball stem of that language (Snowball v3.1.1). |
| `fold` | Strips Latin, Greek, and Cyrillic accents (`é` → `e`, `ü` → `u`, `ß` → `ss`). A preserved character stays. |
| length guard | Last in every built-in. At 255 bytes, `truncate`: the longest prefix that fits on a character boundary. |

Stopword lists are lowercase and still have their accents, so `stop` runs after `lowercase` and before `fold`. Stemmers expect that same form, so `stem` runs before `fold`. Stemming is not idempotent (`accelerated` → `acceler` → `accel` in English). A query is stemmed once, by the field's analyzer.

`edge_ngram` is a filter of SPEC §6.3.1. It replaces a token with its character prefixes and stacks them on that token's position. No built-in analyzer includes it. Synonyms are not a filter of this chain. They are a lookup run when a query is compiled. [Synonym and prefix search](/guide/synonym-and-prefix-search/) is that lookup.

Tokenization follows Unicode 17.0.0. A released analyzer version keeps its chain. A Unicode change that moves a built-in's output ships as a new version (SPEC §6.3.1, ADR-0025). Every built-in below is version 1.

## Tokenizers

Three tokenizers exist. Each built-in analyzer uses one of them.

`standard` splits on Unicode word boundaries (UAX #29) and keeps a segment that contains an alphanumeric character. Punctuation, symbols, and emoji produce no token. A keycap such as `1️⃣` keeps its digit. Numbers with an inner separator stay whole (`3.14`, `1,000`). An apostrophe between letters joins them (`don't`, and `’` as well). `ana@example.com` becomes `ana` and `example.com`. `_` joins (`snake_case`). A colon joins letters on both sides (`key:value`) and splits otherwise (`12:30`). Scripts written without spaces (Han, Hiragana, Thai, and others) fall back to one token per character. Katakana runs and Hangul words stay whole. There is no dictionary segmentation in v1 (ROADMAP 33.1.1).

`whitespace` splits on Unicode `White_Space`. Punctuation stays in the token. `C++` and `Go!` are tokens. Case stays as written, because this tokenizer's analyzer does not lowercase.

`keyword` emits the whole value as one token. An empty value emits none. Spaces stay inside the token: `New York` is one token whose text is `New York`.

## Built-in analyzers

A `text` field that names no analyzer uses `standard`. `AnalyzerRef::builtin(name)` names one of these in code.

| Analyzer | Tokenizer | Filters, in order |
|---|---|---|
| `standard@1` | `standard` | `nfkc`, `lowercase`, length guard |
| `keyword@1` | `keyword` | length guard |
| `whitespace@1` | `whitespace` | length guard |
| `en@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, English `stop`, English stem (Porter2), `fold` (preserves nothing), length guard |
| `es@1` | `standard` | `nfkc`, `lowercase`, Spanish `stop`, Spanish stem, `fold` (preserves `ñ`), length guard |
| `pt@1` | `standard` | `nfkc`, `lowercase`, Portuguese `stop`, Portuguese stem, `fold` (preserves nothing), length guard |
| `fr@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, French `elision`, French `stop`, French stem, `fold` (preserves nothing), length guard |
| `de@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, German `stop`, German stem, `fold` (preserves nothing), length guard |
| `it@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, Italian `elision`, Italian `stop`, Italian stem, `fold` (preserves nothing), length guard |

`standard` lowercases and normalizes. It has no stopword list and no stemmer, and it does not fold accents: `café` stays `café`. `keyword` and `whitespace` do not lowercase, fold, stem, or drop stopwords. Case and accents are part of the term. The guard is why two values of a `keyword`-analyzed field that share their first 255 bytes are one term. Exact matching of a long value is the `keyword` field type, which is not analyzed (SPEC §6.1, §6.4).

The absolute cap is wider than the guard. A token longer than 32 KiB (the keyword limit) is rejected at index time, and dropped from a query. The built-in guard truncates at 255 bytes first, so a built-in chain does not hand the index a token that long.

## Language analyzers

Each language analyzer uses that language's stopword list and that language's Snowball stemmer. English is the classic 33-word list (articles, prepositions, conjunctions, negation). The others are larger closed-class lists: pronouns, possessives, demonstratives, and common auxiliary forms, and they leave out words that are often content words. There is no other language analyzer. A well-formed name this build does not ship, such as the SPEC's example `nl@1` from a newer build, is rejected on a new definition and unsupported when a stored schema names it.

`en` maps `’` to `'` before stemming, so `John’s` and `John's` analyze the same way. `John’s cars` is `john` and `car`. `running` is `run`. Words of fewer than 3 characters are left unstemmed. `of` and `the` are stopwords, and a removed stopword still occupies its position: `statue-of-the` yields one token and three positions.

`es` keeps `ñ`, so `año` and `ano` stay different terms. Acute accents and `ü` fold, so `canción` and `pingüino` meet `cancion` and `pinguino`. `Los años de las canciones` is `años` and `cancion`. Stopwords are matched before folding: `más` and `está` produce no token. `es` has no apostrophe filter and no elision filter.

`pt` folds every accent. `não` is a stopword, and `não ação` is `aca`.

`fr` and `it` run elision before the stopword list, so `c'est` is the stopword `est` and `l'ho` is the stopword `ho`. `L’été dernier, les enfants jouaient dans les forêts de l'Auvergne et c'était magnifique.` under `fr` is `ete`, `derni`, `enfant`, `jou`, `foret`, `auvergn`, `magnif`. `Nell’estate degli anni passati, le città italiane erano piene di turisti velocemente.` under `it` is `estat`, `anni`, `pass`, `citt`, `italian`, `eran`, `pien`, `turist`, `veloc`.

`de` maps `’` as well, for endings such as `Grimm’sche`. `Die Häuser der Straße wurden aufeinanderfolgend renoviert, und Grimm'sche Märchen gelesen.` is `haus`, `strass`, `aufeinanderfolg`, `renoviert`, `grimm`, `march`, `geles`.

Stemming runs before folding, so a query typed without the accent can stem differently from the accented word (`maçã` and `maca` under `pt`). The relaxation cascade is what bridges that miss. [Typo-tolerant search](/guide/typo-tolerant-search/) is the cascade. L0 does not rank the characters as typed above the stem: the index holds the stem.

## Choosing an analyzer

Only a `text` field has an analyzer (SPEC §6.2.2). The option is `"analyzer": "en"` (the current version, today 1) or `"analyzer": "en@1"` (that version, pinned). Omitting it selects `standard`. The engine stores `name@version`. A name this build does not have is rejected (`ErrorCode::InvalidArgument`). A stored schema that names a well-formed version this build lacks, such as `es@2`, is unsupported (`ErrorCode::Unsupported`), the same class as an unknown format feature, and the database does not open.

In code, `AnalyzerRef::builtin("en")` is the unpinned current version, `AnalyzerRef::new("en", 1)` is pinned, and `AnalyzerRef::parse` reads either spelling; each is `None` for a name or version this build does not ship. `TextOptions::analyzer` sets it on a field. SPEC §6.2.2 also lets a schema name an analyzer registered with the engine, but the embedded API takes no registry, so a collection of `persia` names built-in analyzers only. Pluggable tokenizers a collection can register are ROADMAP 33.1.1.

Changing `analyzer` (name or version) or `positions` changes what is indexed. Evolution rejects that change. Another analyzer means a new collection and a reindex (SPEC §6.2.4). An unpinned `"en"` re-applied after a later version exists keeps the version the field already has. `boost`, `k1`, and `b` are query-time and may change without a reindex. How those weights score the analyzed term is [BM25F ranking](/guide/ranking/).

`positions` defaults to false. Phrases and the optional proximity bonus need it. The schema sketch in SPEC §6.2, as JSON:

```json
{
  "title": { "type": "text", "analyzer": "en", "boost": 3.0, "positions": true },
  "body": { "type": "text", "analyzer": "standard" },
  "sku": { "type": "keyword" }
}
```

`sku` is a `keyword` field. `sku:AB-12` matches the text as written. It does not run `keyword@1`. The `keyword` analyzer is a `text`-field pipeline, for a field whose whole value should be one unnormalized term and still be scored as text.

Each string in a multi-valued `text` field is analyzed on its own. The next value starts 100 positions after the end of the previous value, and that end is one past its last token, so a phrase does not run from one value into the next.

## Index time and query time

A document's text is analyzed with every filter of its field's chain. The words of a search, and synonym terms, are analyzed with the same chain minus any filter that stacks tokens (SPEC §6.3.4).

`edge_ngram` is the filter that stacks. An index-time prefix such as `lapt` has to match the query term `lapt` as written. Expanding the query into prefixes would not line up with the analyzed query. No built-in analyzer stacks tokens, so for the nine names above, index-time and query-time analysis are the same pipeline. A field that did stack would stay exact under prefix and fuzzy relaxation, because those steps must not add another prefix on top of the grams. [Synonym and prefix search](/guide/synonym-and-prefix-search/#what-stays-an-exact-term) and [Fuzzy search](/guide/fuzzy-search/) are that rule.

## What the query compiler does with the tokens

When a search runs, `q` is compiled with the analyzers the collection indexes its `text` fields with, and that never fails (SPEC §7.1.1). The grammar and limits are [Typo-tolerant search](/guide/typo-tolerant-search/#compiling-a-query). The analysis rules are these.

A word is analyzed with the query-time analyzer of each default `text` field (the request's `fields`, or every `text` field). A field whose analysis is empty is left out. When the fields agree on spans (no two spans overlap unless they are equal), each span becomes one clause over the fields that produced it, and each span is required. `e-mail` under `standard` is two spans. When the fields split the word apart — `standard` yields `e` and `mail`, `whitespace` keeps `e-mail` — one field's split does not add requirements for another field. Each field requires its own tokens, and one of the fields must match.

A `"phrase"` is, per default field, one term when that field produced one token. `"e-mail"` in a `whitespace` field is that one token, so it is a term. More than one token becomes a phrase when the field stores positions. Relative positions keep the gaps stopwords left. The same tokens on a field without positions require each token. A field with no token is left out, and one of the fields must match. A phrase that is at most one token in every field is compiled as a word. Phrase scoring is [BM25F ranking](/guide/ranking/#clauses).

`field:` before a word or phrase limits it to that field. A `text` field is analyzed as above. A `keyword` field is one term of the text as given, unanalyzed. `-` excludes the clause. A clause that analyzes to nothing (a stopword, or punctuation `standard` drops) is left out. With nothing left, the query matches every document.

Synonym groups are analyzed with that same query-time analysis, per default field, over the required plain words. A phrase, a `field:` clause, or an exclusion ends the run. [Synonym and prefix search](/guide/synonym-and-prefix-search/) is how a run becomes alternatives. Prefix (L3) and fuzzy (L4) then widen the analyzed term, not the characters the user typed. Under `en`, `running` is `run`, three characters, so L4 leaves it exact. [Fuzzy search](/guide/fuzzy-search/#how-a-fuzzy-step-is-built) is the length table. A `keyword` term is not expanded.

Filters on a `keyword` or a number do not analyze. They are [Filtered full-text search](/guide/filtered-search/).

## What is still planned

| Piece | Status |
|---|---|
| Nine built-in analyzers, each at version 1, run at index and query time | Done (ROADMAP M4, 7.1). |
| `edge_ngram` | In the analysis crate. No built-in analyzer uses it. |
| Character filters and span mapping | No built-in analyzer has a character filter. |
| A custom analyzer named by a collection of `persia` | The engine reads one (SPEC §6.2.2); the embedded API takes no registry. |
| Dictionary segmentation for scripts UAX #29 splits per character | Not in v1 (ROADMAP 33.1.1). The per-character fallback is what the `standard` tokenizer does today. |
| A Turkish or Azeri analyzer | Not built. Lowercasing is the Unicode default: `I` → `i`, `İ` → `i` plus a combining dot. |
| Highlighted fragments aligned to analyzer spans | Not written (ROADMAP 17.2.1). |

## FAQ

### Does Persia DB tokenize and stem full-text search in Rust today?

Yes. Each `text` field names an analyzer in its collection's schema, and the same analyzer, at the same version, analyzes the documents put into it and the words of a search. Nine built-in analyzers ship, each at version 1. %%MATCHING_CELL%%

### Which analyzers are built in?

`standard`, `keyword`, `whitespace`, `en`, `es`, `pt`, `fr`, `de`, and `it`, each at version 1. `standard` is Unicode word boundaries, NFKC, and lowercase, with no stopwords and no stemmer. `keyword` keeps the whole value as one token. `whitespace` splits on Unicode whitespace and keeps case and punctuation. `en`, `es`, `pt`, `fr`, `de`, and `it` add that language's stopword list and Snowball stemmer, then accent folding. `es` is the one that keeps `ñ`.

### Do keyword and whitespace analyzers stem or lowercase?

No. Each one's only token filter is the length guard, which truncates a token past 255 bytes. `keyword` emits the whole value, spaces included, and emits nothing for an empty value. `whitespace` splits on Unicode whitespace and leaves punctuation in the token. Case stays as written in both.

### Does query text use the same analyzer as the indexed field?

Yes for every built-in analyzer. Query analysis skips only filters that stack tokens. The only such filter is `edge_ngram`, and no built-in analyzer includes it, so index-time and query-time tokens match.

### Can I call Db::open and choose an analyzer?

Yes. A `text` field names a built-in analyzer in the collection definition, as `"en"` (its current version) or `"en@1"` (pinned), or in code as `TextOptions::analyzer` with an `AnalyzerRef`. The embedded API has no way to register a custom analyzer yet.

## Read next

- [BM25F ranking](/guide/ranking/) — how an analyzed term is scored, and how a phrase uses positions.
- [Synonym and prefix search](/guide/synonym-and-prefix-search/) — synonym terms analyzed with the query-time pipeline, and L3 prefix expansion of those terms.
- [Fuzzy search and did-you-mean](/guide/fuzzy-search/) — L4 edits the analyzed term, so a stem's length is what the distance sees.
- [Typo-tolerant search](/guide/typo-tolerant-search/) — the compiler and the cascade that runs when the analyzed query is short of hits.
- [Filtered full-text search](/guide/filtered-search/) — `keyword` and numeric filters match stored values, not analyzer output.
- [Feature reference](/guide/features/) — the status table, including the analyzer row.
- [Getting started](/guide/getting-started/) — open a database, add documents, and search.
- [FAQ](/guide/faq/) — production readiness, the file format, and the license.
