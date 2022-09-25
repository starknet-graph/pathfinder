# `pathfinder` Fork with Firehose Support

This is a [`pathfinder`](https://github.com/eqlabs/pathfinder) fork with support for the [Firehose protocol](https://firehose.streamingfast.io/), which in turn enables StarkNet support in [The Graph](https://thegraph.com/). It's created and maintained by the [zkLend](https://zklend.com/) team.

This fork syncs the `main` branch with the upstream continuously, after which it applies a single commit replacing the `README.md` file with the one you're reading right now. The actual code for Firehose support lives in another branch `patch`, which always builds on the latest `main` branch.

Whenever a version is released on the upstream project, we will make the same release except with the patch applied. Our release would essentially be the patch branch rebased on the corresponding upstream tag.
