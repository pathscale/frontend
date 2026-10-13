# Pending work and branch cleanup,13 October2026

This is a visible work queue, not a backup directory. Everything here is committed
on the existing PR. Source already integrated on master or represented by a PR
needs no backup. Consumed source snapshots and opaque history bundles are removed.
No correctness or merge readiness is claimed.

## Branch disposition

| Original ref | Tip | Disposition |
| --- | --- | --- |
| refs/heads/arena-ast | 9aeed044369f080af18165a397a276728b2a3679 | Included in pending series for refs/heads/parse-speed-2 |
| refs/heads/codex/ac-trait-visibility | 5bb739921e1607fa958c24230e715fd13c671077 | on-master |
| refs/heads/parse-speed-2 | 749498a94dfd72e2dfe9896549bb36d961b01928 | patches/pending-parse-speed-2-20261013.mbox; base ef4d46df22dc268b818f948623ece168b8295dce; needs integration review |
| refs/heads/scratch/compound-1956029 | b74c53efc7fce9de994ffd7b682f12170b7a4e08 | patches/pending-scratch-compound-1956029-20261013.mbox; base ef4d46df22dc268b818f948623ece168b8295dce; needs integration review |

Mailbox files contain original commit patches in readable form, preserving commit
boundaries. Review their base and overlapping source before applying; do not blindly
replay an old branch over the current code. Paired .commits.txt files identify the
exact source commits. Existing self-hosting proposals remain separately labelled.

Work from local master. Push only HEAD to the existing PR head with an explicit
refspec. Keep no extra local branches or source worktrees. No merge or squash was
performed during this preservation operation.


## Prior PR13 stage/parser work

`pending-pr13-stage-parser-20261013.mbox` preserves the one unique former PR13
commit 85a246fb862e353f630a9d04cac08620945e2c21, based on
630da40232621e3f2b3a560d2f205ccf1cd227b1. It includes stage/parser work and
parallel timing material. It is not integrated or validated. Review overlap with
current source before applying. This readable pending proposal replaces the
removed opaque bundle; no alternate checkout or branch is needed.
