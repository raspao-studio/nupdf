# Arlington PDF Model, selected definitions

These files are from the PDF Association's Arlington PDF Model,
<https://github.com/pdf-association/arlington-pdf-model>, at commit
`603c174b024d1d2bb15a33686af83800d5c41269`, directory `tsv/latest`. They are unmodified. The model is
licensed under the Apache License 2.0; its text is in `LICENSE` here.

The harness's `arlington-*` checks and `cargo xtask arlington` read them
(`xtask/src/arlington.rs`), applying the rules that need no predicates.
Only the definitions for the dictionaries nupdf writes or edits are
included.
