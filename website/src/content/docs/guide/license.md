---
title: "License"
description: "Persia DB is licensed under the Apache License, Version 2.0, including copyright, NOTICE attribution, and the terms for contributions."
head:
  - tag: title
    content: "Apache License 2.0 for the search database | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Apache License 2.0 for the search database | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Apache License 2.0 for the search database | PersiaDB"
---

Persia DB is licensed under the [Apache License, Version 2.0](https://github.com/tomiok/persiaDB/blob/main/LICENSE).
The repository has one `LICENSE` file, and that file is the Apache License, Version 2.0.
The same text is published at [apache.org/licenses/LICENSE-2.0](https://www.apache.org/licenses/LICENSE-2.0).
Copyright and attribution live in [`NOTICE`](https://github.com/tomiok/persiaDB/blob/main/NOTICE):

> Persia DB
> Copyright 2026 The Persia DB Authors
>
> This product includes software developed by The Persia DB Authors.
> Licensed under the Apache License, Version 2.0 (see LICENSE).

The workspace records the same license in the root `Cargo.toml` (`license = "Apache-2.0"`).
A short answer is also on the [FAQ](/guide/faq/).

This page describes what that license says. It is not legal advice.

## Embedding in an application

The license grants a copyright license (section 2) and a patent license (section 3).
Both are perpetual, worldwide, non-exclusive, no-charge, and royalty-free.
It does not split commercial use from other use, and it does not ask for a separate paid license to embed the library.
The same grant is what an open-source application and a commercial application are both reading.

The copyright grant covers reproducing the work, preparing derivative works of it, and distributing it in source form or object form.
That is the clause that covers shipping Persia DB inside another program.

If you redistribute Persia DB, or a derivative of it, section 4 requires all of the following:

- Give recipients a copy of the Apache License, Version 2.0.
- State, in any file you changed, that you changed it.
- Keep the copyright, patent, trademark, and attribution notices from the source, except notices that do not apply to the part you kept.
- Keep the attribution notices from `NOTICE`. The license lists where that copy can live: a NOTICE file shipped with the derivative, the source or documentation, or a display where such third-party notices normally appear.

Section 1 defines a derivative work. It says that definition does not include a work that remains separable from Persia DB, or that merely links (or binds by name) to its interfaces.
An application that calls the library and keeps its own code separate is, under that sentence, not itself a derivative work.
The license still applies to the Persia DB code distributed with that application: recipients of that code need the license, the notices, and `NOTICE`.

Section 4 also says you may add your own copyright statement to your modifications, and you may apply additional or different license terms to your modifications, or to a derivative work as a whole, provided your use of the original work still meets the conditions above.

The patent grant ends for the party who files patent litigation claiming that the work, or a contribution incorporated in it, constitutes patent infringement (section 3).
Section 6 does not grant trademark rights in the project's names, except the reasonable and customary use needed to describe the origin of the work and to reproduce `NOTICE`.
Sections 7 and 8 state that the work is provided as is, without warranty, and they limit contributor liability.
Section 7 also says you are responsible for deciding whether using or redistributing the work is appropriate.

## Contributions

Unless a contributor explicitly states otherwise, a contribution intentionally submitted for inclusion in Persia DB is under this same license, without additional terms (section 5).
That sentence does not replace a separate license agreement already made with the project.
How to send a change is on the [contributing](/guide/contributing/) page.
By participating you also agree to the [Code of Conduct](https://github.com/tomiok/persiaDB/blob/main/CODE_OF_CONDUCT.md).

## Other licenses in the tree

`LICENSE` is the license for Persia DB. Third-party crates keep their own licenses.
[`deny.toml`](https://github.com/tomiok/persiaDB/blob/main/deny.toml) allows a set of permissive licenses for that dependency graph, including Apache-2.0 and MIT, and `cargo deny check` enforces the list.
Those terms belong to the dependencies. They are not a second license on Persia DB.
The fuzz-only `libfuzzer-sys` crate is an exception in that file (NCSA) and is not part of the database.
