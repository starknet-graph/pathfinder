# `pathfinder` Fork with Firehose Support

This is a [`pathfinder`](https://github.com/eqlabs/pathfinder) fork with support for the [Firehose protocol](https://firehose.streamingfast.io/), which in turn enables StarkNet support in [The Graph](https://thegraph.com/). It's created and maintained by the [zkLend](https://zklend.com/) team.

Powered by GitHub Actions, this fork syncs with the upstream continuously:

- The [`main`](https://github.com/starknet-graph/pathfinder/tree/main) branch in this fork is the same as the upstream `main` branch, except with necessary changes to GitHub Actions workflows and the README file you're reading right now.

- Then, the actual code changes for enabling Firehose support live in the [`patch`](https://github.com/starknet-graph/pathfinder/tree/patch) branch, which is always rebased on the `main` branch in this fork.

Contributions are welcomed! Any contribution should be made to the [`patch`](https://github.com/starknet-graph/pathfinder/tree/patch) branch, as any changes to the `main` branch would be overwritten by the syncing process.

Whenever a version is released on the upstream project, we will make the same release except with the patch applied. Our release would essentially be the patch branch rebased on the corresponding upstream tag.
