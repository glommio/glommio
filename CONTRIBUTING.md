# Contributing

The best way to interact with this project is and will be a work in progress. However, there are some core tenets based
on my ([@glommer](https://github.com/glommer)) previous experiences (two decades of Linux and Linux-like projects) that
may be summarized as follows:

1. Code review is paramount but not enough to block progress on anyone's end. **What is more important is architectural
   review and unit tests.**
2. Splitting your changes in easy-to-consume, self-contained pieces is important but not enough to impose a constant
   burden on the code author.

Writing system-level software is challenging enough as it is; the last thing we want is to add an undue burden on
contributors.

If you are interested in understanding these motivations a bit better, you can check out
[this article.](https://medium.com/@glaubercosta_11125/the-linux-development-process-is-it-worth-the-hassle-4f09d7ff09a2)

## Tooling and Formatting

This project uses several tools to maintain code quality and consistency:

### Cargo Sort

We use `cargo sort` to keep our dependencies sorted in `Cargo.toml` files. This ensures consistent ordering for better readability and easier diff views.

Install with:

```bash
cargo install cargo-sort
```

### Taplo

We use [Taplo](https://taplo.tamasfe.dev/) for formatting and validating TOML files, including `Cargo.toml`. It ensures consistent formatting across all TOML configurations in this repository.

Install with:

```bash
cargo install taplo-cli --locked
```

### ClIFF

We use [ClIFF](https://github.com/orhun/cliff) for generating changelogs. This tool helps maintain consistent and automated changelog generation from git history.

Install with:

```bash
cargo install git-cliff
```

## Pull Request Process

As of today (September 2021), this is the set of rules that materialize the principles above:

1. **Test your changes.** Unless what you are doing is _absolutely_ trivial, add unit tests. Good unit tests come in
   bundles, and should test for both the expected and unexpected cases.

2. **Discuss your architecture first.** If you are making changes that add new components, new data structures, or
   reorganize an existing flow, it is helpful to plan how best to achieve that. A GitHub issue is the place to have this
   conversation.

3. **Invest in your git log.** If you fix a bug, tell us more about how you found it, in which circumstances it appears,
   etc. This is important for others as well as for _future you_. Split significant changes in smaller, self-contained
   commits, so it is easier to follow what you have done. `git add -p` is a powerful tool, and you should use it.

   At the end, if the result is too hard to follow and the change is simple and limited in complexity, **squashing your
   commits is okay**. Otherwise, if the diff is complex or has a large surface area, we will ask you to rewrite history
   to preserve the individual commits of your branch.

4. **AI-generated contributions are your responsibility.** See [AI-generated contributions](#ai-generated-contributions)
   below.

## AI-generated contributions

AI-assisted work is welcome; the human contributor owns every line of the diff and the pull request as a whole. These
rules exist because review time is the scarcest resource in the project, and unedited AI output spends it quickly.

Before you request review:

1. **Re-read the entire diff yourself and clean out AI slop.** Boilerplate praise and summaries, comments that restate
   the code next to it, speculative error handling for cases that cannot happen, filler doc comments, noise commits —
   remove them. The code must read as if written intentionally by someone who understands it. Note that this repository
   enforces a [comment policy](dylint.toml): prose belongs in `///`/`//!` doc comments, and plain `//` comments are
   rejected by CI. The lint only catches comment noise; the rest of the slop needs human editing.
2. **PR descriptions and GitHub comments are human-readable.** Write concise plain prose: what and why, how it was
   tested, links to issues. No raw AI transcripts, no prompt dumps, no "here is what I did" filler. Your audience is
   human reviewers with limited time, not a chat log.
3. **Do not outsource judgment.** If you cannot explain a change in your own words, you are not ready to open the PR.
4. **Say which AI produced the work.** Every AI-assisted commit carries an `Assisted-by: <tool and model>`
   trailer (e.g. `Assisted-by: claude-sonnet-4-5`); when history is squashed on merge, state the tooling in the PR
   description instead. A commit with no AI involvement at all declares `Assisted-by: none` — CI requires every
   commit to state which it is, since it cannot infer AI involvement. Reviewers deserve to know whose output they
   are reading.

## Licensing

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as
defined in the Apache-2.0 license, shall be dual licensed as described in the [README](README.md#license), without any
additional terms or conditions.
