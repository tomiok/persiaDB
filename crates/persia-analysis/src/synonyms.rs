//! Query-time synonym lookup (SPEC §6.3.2, §7.6 L1).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::token::Token;

/// The synonym groups of a collection, analyzed, for expanding queries (SPEC §6.3.2, §7.6 L1).
///
/// Not a [`TokenFilter`](crate::TokenFilter): a term may be several tokens and expands into alternatives the query
/// compiler ORs, which a flat token stream cannot express. Each term of each group is given as the tokens the
/// field's analyzer makes of it; a term without tokens is ignored, and terms with equal texts and position gaps are
/// one term.
///
/// ```
/// use persia_analysis::{LowercaseFilter, SynonymMap, Token, TokenFilter, Tokenizer, WhitespaceTokenizer};
///
/// let analyze = |text: &str| {
///     let mut tokens = Vec::new();
///     WhitespaceTokenizer.tokenize(text, &mut tokens);
///     LowercaseFilter.apply(&mut tokens);
///     tokens
/// };
/// let map = SynonymMap::new([vec![analyze("TV"), analyze("television set")]]);
/// let query = analyze("cheap tv deals");
/// let matches = map.find(&query);
/// assert_eq!(matches.len(), 1);
/// assert_eq!(matches[0].tokens(), 1..2);
/// let alternatives: Vec<Vec<&str>> = map
///     .alternatives(&matches[0])
///     .iter()
///     .map(|term| term.iter().map(|t| t.text.as_str()).collect())
///     .collect();
/// assert_eq!(alternatives, [["television", "set"]]);
/// ```
#[derive(Debug, Clone, Default)]
pub struct SynonymMap {
    /// Distinct terms, as analyzed.
    terms: Vec<Vec<Token>>,
    /// Per term, the groups containing it, ascending.
    groups_of_term: Vec<Vec<usize>>,
    /// Per group, its distinct terms in the order given.
    groups: Vec<Vec<usize>>,
    /// A trie over terms keyed by token text and position gap to the previous token; node 0 is the root.
    trie: Vec<Node>,
}

#[derive(Debug, Clone, Default)]
struct Node {
    /// Per token text, the children by position gap (almost always one).
    children: HashMap<Box<str>, Vec<(u32, usize)>>,
    term: Option<usize>,
}

/// A run of query tokens equal to a synonym term (see [`SynonymMap::find`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SynonymMatch {
    tokens: Range<usize>,
    term: usize,
}

impl SynonymMatch {
    /// The indexes of the matched tokens in the slice given to [`SynonymMap::find`].
    pub fn tokens(&self) -> Range<usize> {
        self.tokens.clone()
    }
}

impl SynonymMap {
    /// Builds the map from groups of analyzed terms.
    pub fn new<G, T>(groups: G) -> Self
    where
        G: IntoIterator<Item = T>,
        T: IntoIterator<Item = Vec<Token>>,
    {
        let mut map = Self {
            trie: vec![Node::default()],
            ..Self::default()
        };
        for (group_id, group) in groups.into_iter().enumerate() {
            let mut members = Vec::new();
            for term in group {
                let Some(term_id) = map.insert(term) else {
                    continue;
                };
                if !members.contains(&term_id) {
                    members.push(term_id);
                    // Group ids only grow, so each list stays ascending and duplicate-free.
                    if let Some(groups) = map.groups_of_term.get_mut(term_id) {
                        groups.push(group_id);
                    }
                }
            }
            map.groups.push(members);
        }
        map
    }

    /// Inserts a term into the trie and returns its id, or `None` for a term without tokens.
    fn insert(&mut self, term: Vec<Token>) -> Option<usize> {
        let mut node = 0;
        for (gap, text) in gap_keys(&term)? {
            let next = self.trie.len();
            let children = self
                .trie
                .get_mut(node)?
                .children
                .entry(text.into())
                .or_default();
            node = if let Some(&(_, child)) = children.iter().find(|&&(g, _)| g == gap) {
                child
            } else {
                children.push((gap, next));
                self.trie.push(Node::default());
                next
            };
        }
        let next_term = self.terms.len();
        let slot = &mut self.trie.get_mut(node)?.term;
        if let Some(existing) = *slot {
            return Some(existing);
        }
        *slot = Some(next_term);
        self.terms.push(term);
        self.groups_of_term.push(Vec::new());
        Some(next_term)
    }

    /// Finds the synonym terms in `tokens`, an analyzed query: scanning left to right, the longest term starting at
    /// each token matches and its tokens are skipped, so matches are in order and never overlap.
    pub fn find(&self, tokens: &[Token]) -> Vec<SynonymMatch> {
        let mut matches = Vec::new();
        let mut start = 0;
        while start < tokens.len() {
            match self.longest_at(tokens, start) {
                Some((end, term)) => {
                    matches.push(SynonymMatch {
                        tokens: start..end,
                        term,
                    });
                    start = end;
                }
                None => start = start.saturating_add(1),
            }
        }
        matches
    }

    /// The longest term starting at `tokens[start]`: one past its last token, and its id.
    fn longest_at(&self, tokens: &[Token], start: usize) -> Option<(usize, usize)> {
        let mut node = self.trie.first()?;
        let mut longest = None;
        let mut previous = tokens.get(start)?.position;
        for (end, token) in (start..).zip(tokens.get(start..)?) {
            let gap = if end == start {
                0
            } else {
                token.position.checked_sub(previous)?
            };
            previous = token.position;
            let Some(&(_, child)) = node
                .children
                .get(token.text.as_str())
                .and_then(|children| children.iter().find(|&&(g, _)| g == gap))
            else {
                break;
            };
            node = self.trie.get(child)?;
            if let Some(term) = node.term {
                longest = Some((end.checked_add(1)?, term));
            }
        }
        longest
    }

    /// The terms a match expands to: in group order, then term order, the other terms of every group containing the
    /// matched term, each once. Each is given as analyzed (its spans index the term's own text).
    pub fn alternatives(&self, matched: &SynonymMatch) -> Vec<&[Token]> {
        let mut seen = HashSet::from([matched.term]);
        let mut alternatives = Vec::new();
        for &group in self.groups_of_term.get(matched.term).into_iter().flatten() {
            for &term in self.groups.get(group).into_iter().flatten() {
                if seen.insert(term)
                    && let Some(tokens) = self.terms.get(term)
                {
                    alternatives.push(tokens.as_slice());
                }
            }
        }
        alternatives
    }
}

/// The trie keys of an analyzed term: each token's text with its position gap to the previous token (0 for the
/// first), or `None` for a term without tokens.
fn gap_keys(term: &[Token]) -> Option<Vec<(u32, &str)>> {
    let first = term.first()?;
    let mut previous = first.position;
    let mut keys = Vec::with_capacity(term.len());
    for token in term {
        keys.push((token.position.checked_sub(previous)?, token.text.as_str()));
        previous = token.position;
    }
    Some(keys)
}
