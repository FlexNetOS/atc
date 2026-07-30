# Changelog

## [0.1.15](https://github.com/FlexNetOS/atc/compare/v0.1.14...v0.1.15) (2026-07-29)


### Documentation

* **skills:** add a verify skill capturing the atc CLI verification recipe ([988b77f](https://github.com/FlexNetOS/atc/commit/988b77faacbb62c4426f12a8f99457c50277f9ad))

## [0.1.14](https://github.com/FlexNetOS/atc/compare/v0.1.13...v0.1.14) (2026-07-29)


### Bug Fixes

* **atc:** reconcile the landed purged branches into a building workspace ([f433d40](https://github.com/FlexNetOS/atc/commit/f433d40c5aff8a2c42153923ec507689bc68d7bc))

## [0.1.13](https://github.com/FlexNetOS/atc/compare/v0.1.12...v0.1.13) (2026-07-29)


### Bug Fixes

* **ci:** make release-please bump Cargo.lock, and fail the build when it doesn't ([1697e6d](https://github.com/FlexNetOS/atc/commit/1697e6d3aeb96524d851b105e53d071f49f5de27))

## [0.1.12](https://github.com/FlexNetOS/atc/compare/v0.1.11...v0.1.12) (2026-07-29)


### Miscellaneous

* sync Cargo.lock with the 0.1.11 release version bump ([fe5ed8a](https://github.com/FlexNetOS/atc/commit/fe5ed8a126e1cc6e9fa0e46a58ad28db65fe95d5))

## [0.1.11](https://github.com/FlexNetOS/atc/compare/v0.1.10...v0.1.11) (2026-07-29)


### Miscellaneous

* sync Cargo.lock with the 0.1.10 release version bump ([e062b41](https://github.com/FlexNetOS/atc/commit/e062b417a6552dc96d9fa8822cc780dc904b5c1c))

## [0.1.10](https://github.com/FlexNetOS/atc/compare/v0.1.9...v0.1.10) (2026-07-29)


### Miscellaneous

* **atc:** land outstanding working-tree work ([2a923a6](https://github.com/FlexNetOS/atc/commit/2a923a6abc89872993face96f86ef0a0850fd688))
* sync handoff harness assets ([#6](https://github.com/FlexNetOS/atc/issues/6)) ([f5a0168](https://github.com/FlexNetOS/atc/commit/f5a01681be582980e52a940e6a515158e695a015))

## [0.1.9](https://github.com/FlexNetOS/atc/compare/v0.1.8...v0.1.9) (2026-06-27)


### Bug Fixes

* **ci:** run PR workflows on ubuntu-latest (no self-hosted macOS runner) ([#3](https://github.com/FlexNetOS/atc/issues/3)) ([5d652d7](https://github.com/FlexNetOS/atc/commit/5d652d7d3f8cf4eddbbfcb98a842801085595d39))
* re-point org refs gitkb/harmony-labs -&gt; FlexNetOS after migration ([310a1f9](https://github.com/FlexNetOS/atc/commit/310a1f9a9596f7fbd0b1b86766cc2a22b0cf38b7))


### Miscellaneous

* apply handoff fleet deployment sync ([#5](https://github.com/FlexNetOS/atc/issues/5)) ([8434941](https://github.com/FlexNetOS/atc/commit/84349418e7807461f03e0257b72d1ab68d19aaa2))
* deploy rusty-idd thin-adapter control plane (fleet) ([#4](https://github.com/FlexNetOS/atc/issues/4)) ([909ad8c](https://github.com/FlexNetOS/atc/commit/909ad8c9f298abddb305271b958089ffc633988e))
* seed .handoff continuity layer (P7) ([#2](https://github.com/FlexNetOS/atc/issues/2)) ([970ee9e](https://github.com/FlexNetOS/atc/commit/970ee9e3b80f465764a99f24f1213507b9368ae2))
* update Cargo.lock ([831d594](https://github.com/FlexNetOS/atc/commit/831d5943b0690f77b6306d99c6d09919f5a06a88))

## [0.1.8](https://github.com/gitkb/atc/compare/v0.1.7...v0.1.8) (2026-06-05)


### Features

* add ATC tmux session locators and open-session ([#72](https://github.com/gitkb/atc/issues/72)) ([e2d7147](https://github.com/gitkb/atc/commit/e2d714756ed6a0b5e9ab970130be4e8d147976c7))

## [0.1.7](https://github.com/gitkb/atc/compare/v0.1.6...v0.1.7) (2026-06-05)


### Features

* add ATC session switchboard ([#69](https://github.com/gitkb/atc/issues/69)) ([84f4228](https://github.com/gitkb/atc/commit/84f422813c1d965d77b1dd258a4085519d14ad55))


### Tests

* harden atc sessions review follow-up ([#71](https://github.com/gitkb/atc/issues/71)) ([692f98b](https://github.com/gitkb/atc/commit/692f98b7c561df088bd0ddb7d03e3d4aee92e200))

## [0.1.6](https://github.com/gitkb/atc/compare/v0.1.5...v0.1.6) (2026-06-03)


### Features

* add durable agent session metadata ([#64](https://github.com/gitkb/atc/issues/64)) ([a4ac948](https://github.com/gitkb/atc/commit/a4ac948449745ad008fd180739d50011b3f7215c))
* add composable ATC run resume ([#65](https://github.com/gitkb/atc/issues/65)) ([75c3e35](https://github.com/gitkb/atc/commit/75c3e35d84f18f0de5597a0894129fbe350c3910))


### Bug Fixes

* **release:** include all conventional types ([#67](https://github.com/gitkb/atc/issues/67)) ([e11d953](https://github.com/gitkb/atc/commit/e11d95300c1a814ca7905e64bbc528f88c67c201))


### CI

* add semantic PR title checks ([#66](https://github.com/gitkb/atc/issues/66)) ([37dafee](https://github.com/gitkb/atc/commit/37dafeef486d73130d6df48cee230c095fb1d629))

## [0.1.5](https://github.com/gitkb/atc/compare/v0.1.4...v0.1.5) (2026-04-26)


### Features

* **cli:** atc status grouped table — id column, reorder, dynamic alignment ([#59](https://github.com/gitkb/atc/issues/59)) ([d27e033](https://github.com/gitkb/atc/commit/d27e033a13337009a7cbf2a9eb7fed0d604afde5))

## [0.1.4](https://github.com/gitkb/atc/compare/v0.1.3...v0.1.4) (2026-04-25)


### Bug Fixes

* **cli:** atc status — quiet WARN, colors through pager, chop long lines ([#57](https://github.com/gitkb/atc/issues/57)) ([4744e05](https://github.com/gitkb/atc/commit/4744e05a4ebecf34497c78da195bdadbde8631e9))

## [0.1.3](https://github.com/gitkb/atc/compare/v0.1.2...v0.1.3) (2026-04-25)


### Features

* bake skills into atc init + add atc init &lt;agent&gt; wiring ([#53](https://github.com/gitkb/atc/issues/53)) ([ed81e96](https://github.com/gitkb/atc/commit/ed81e96e0b24d6fba7e22fdba2e0ace2e868febc))
* **cli:** human-friendly atc output (pager, colors, sort, hint text) ([#54](https://github.com/gitkb/atc/issues/54)) ([ddc3346](https://github.com/gitkb/atc/commit/ddc3346a602b3f177d4959481b148071ff4c77a7))

## [0.1.2](https://github.com/gitkb/atc/compare/v0.1.1...v0.1.2) (2026-04-11)


### Bug Fixes

* updating harmony-labs GitHub org to gitkb ([#51](https://github.com/gitkb/atc/issues/51)) ([568e8f1](https://github.com/gitkb/atc/commit/568e8f181a4b23a358be9aba2e8f7cf65799e20e))

## [0.1.1](https://github.com/gitkb/atc/compare/v0.1.0...v0.1.1) (2026-03-30)


### Features

* .atc/ directory convention with directives, templates, components ([#28](https://github.com/gitkb/atc/issues/28)) ([c9062a9](https://github.com/gitkb/atc/commit/c9062a9bcddf3c7eb95d8915c7e8c435b130e294))
* add continuous dispatch via queue, daemon, and sources ([#27](https://github.com/gitkb/atc/issues/27)) ([0605219](https://github.com/gitkb/atc/commit/06052198114d1fd9039844e11db0061e1e2ed224))
* add release-please for automated version management ([#48](https://github.com/gitkb/atc/issues/48)) ([2446101](https://github.com/gitkb/atc/commit/24461017f89b410c08b4967f39d1b285b8e5d27f))
* atc dispatch — worktree, agent spawn, and registry record ([bbc5148](https://github.com/gitkb/atc/commit/bbc5148d36e5d15ebca4bbd15ab055bad4dccb34))
* atc dispatch — worktree, agent spawn, and registry record ([a3250fa](https://github.com/gitkb/atc/commit/a3250faa607eef5624bef8eb54c5c21029da5b47))
* atc health — six-signal health checks ([#6](https://github.com/gitkb/atc/issues/6)) ([011b968](https://github.com/gitkb/atc/commit/011b968c9910c917843d5e174f968c0f30a7112e))
* ATC repo scaffold, crate structure, and SQLite registry foundation ([94e96f1](https://github.com/gitkb/atc/commit/94e96f1b32099ca1f99cb7b55c9d43c412b0cb02))
* **atc:** add stop and cleanup commands ([#9](https://github.com/gitkb/atc/issues/9)) ([52864fc](https://github.com/gitkb/atc/commit/52864fcf76e04967b41c42e34d1dcaffce9cfb4d))
* **atc:** ATC Phase 0 — Registry + Dispatch Foundation Fixes ([#8](https://github.com/gitkb/atc/issues/8)) ([d591b90](https://github.com/gitkb/atc/commit/d591b9041639be9ae08a01637dfd3e471ed5c5f3))
* **atc:** ATC Phase 2 — Post-Completion Pipeline ([#10](https://github.com/gitkb/atc/issues/10)) ([5f68206](https://github.com/gitkb/atc/commit/5f68206596985cdc1a8f1e6e6c2f1d1bf4b06027))
* **atc:** ATC Phase 6 — Multi-KB Discovery + Per-Project Environment ([#17](https://github.com/gitkb/atc/issues/17)) ([3ec35a1](https://github.com/gitkb/atc/commit/3ec35a19bfc39714c7402291b7bb018e88d244ea))
* **atc:** InputResolver + DispatchPipeline unification (Phase 4B) ([#15](https://github.com/gitkb/atc/issues/15)) ([4d432b7](https://github.com/gitkb/atc/commit/4d432b78a216e5fa407f07ad1e1d30c74a4426ae))
* **config:** discover atc.toml via upward directory traversal ([#21](https://github.com/gitkb/atc/issues/21)) ([ea1d92c](https://github.com/gitkb/atc/commit/ea1d92c4dc8d51cc96c5982d568ebd8f3acaf2e7))
* externalize prompt templates with mode config overrides ([#3](https://github.com/gitkb/atc/issues/3)) ([f52eb4c](https://github.com/gitkb/atc/commit/f52eb4ca066320c9a32d5f6c9af84637c3cbb46c))
* full template content, partials, JSONL watch, branch sanitization ([#35](https://github.com/gitkb/atc/issues/35)) ([4a4e936](https://github.com/gitkb/atc/commit/4a4e936abd58c61a69236672b916f30f8f07511b))
* **health:** ATC Phase 7 — Health Check Enhancements ([#14](https://github.com/gitkb/atc/issues/14)) ([4542675](https://github.com/gitkb/atc/commit/45426751392c0d1af5de6ebe609177e454c96e54))
* lightweight execution path — atc quick for ephemeral AI calls ([#41](https://github.com/gitkb/atc/issues/41)) ([7350202](https://github.com/gitkb/atc/commit/7350202d9ecb3464168d438461d056158c197080))
* multi-repo dispatch — one task, N repos, N PRs ([#31](https://github.com/gitkb/atc/issues/31)) ([87d09a3](https://github.com/gitkb/atc/commit/87d09a31ca8e4bbd262ea0c26c39f13f3c70e897))
* operational commands — close, redirect, retry, status, info, logs ([#7](https://github.com/gitkb/atc/issues/7)) ([6c74078](https://github.com/gitkb/atc/commit/6c74078f08df4476f21b8bc2531ea0118c23bf77))
* PR workflow parity with dispatch.sh ([#30](https://github.com/gitkb/atc/issues/30)) ([630014b](https://github.com/gitkb/atc/commit/630014b377c8f17e4a19dc7afa906210fe04a89b))
* **prompt:** add prompt engine with Handlebars rendering ([#13](https://github.com/gitkb/atc/issues/13)) ([b66d934](https://github.com/gitkb/atc/commit/b66d934d83569d0cc08ae16ee0bbdd5a4c11b68f))
* **providers:** ATC Phase 5 — Context Providers ([#16](https://github.com/gitkb/atc/issues/16)) ([ef17aa7](https://github.com/gitkb/atc/commit/ef17aa755da2bac3e8621f51733ca101eacdcadd))
* **release:** Homebrew distribution (tap + CI release) ([#18](https://github.com/gitkb/atc/issues/18)) ([afecc3a](https://github.com/gitkb/atc/commit/afecc3a5996e18fb4bd87a4f60293658bc61c426))
* **retry:** adaptive budget/turns adjustment based on failure subtype ([#12](https://github.com/gitkb/atc/issues/12)) ([c1ffdf5](https://github.com/gitkb/atc/commit/c1ffdf55fea2b291e2ec29bbbfbff08cbd5daa08))
* scaffold ATC workspace with core types, SQLite registry, and CLI skeleton ([e8a5b7a](https://github.com/gitkb/atc/commit/e8a5b7a51fd8a011aee72067c94854aa93021657))
* self-contained triage.md — full comment text + pre-built commands ([#45](https://github.com/gitkb/atc/issues/45)) ([4bd19a2](https://github.com/gitkb/atc/commit/4bd19a2fce8a3df04f02ae563be7403990a87eb3))
* wire templates to directives — system prompt + provider composition ([#29](https://github.com/gitkb/atc/issues/29)) ([6f42da0](https://github.com/gitkb/atc/commit/6f42da0ebd791449ab8301e5e23b7dd0a0ef63b6))
* work units — lifecycle grouping for dispatches, PRs, branches ([#36](https://github.com/gitkb/atc/issues/36)) ([483c17c](https://github.com/gitkb/atc/commit/483c17cd66cd5d7b1b74dc3997dede3d122a6716))
* worktree routing policy — document-aware CWD resolution ([#43](https://github.com/gitkb/atc/issues/43)) ([6fee568](https://github.com/gitkb/atc/commit/6fee568f45974fb2505374e48233790442304505))


### Bug Fixes

* add --version flag to atc CLI ([#47](https://github.com/gitkb/atc/issues/47)) ([6299cf4](https://github.com/gitkb/atc/commit/6299cf4c50f193108abdf660b3927fbc064d7ae3))
* address all 5 PR review comments ([55fe932](https://github.com/gitkb/atc/commit/55fe9328f3cf0352327b7fca039bd961de7950d5))
* address all PR review comments ([2f64f77](https://github.com/gitkb/atc/commit/2f64f777940f1ec20bfdb42af7ce1aef9eaff781))
* address remaining review comments (automated + manual) ([a35078c](https://github.com/gitkb/atc/commit/a35078c47dedba5dd8f9196d7886e16c80a08551))
* address round 5 CodeRabbit review comments ([1b56ea0](https://github.com/gitkb/atc/commit/1b56ea09d8b5658702d6457c870c9305fdcdb10e))
* address round-2 PR review comments ([a3352a7](https://github.com/gitkb/atc/commit/a3352a782161e5ec2c0c1eb051236af7730ef8b2))
* address round-3 PR review comments ([77d4a39](https://github.com/gitkb/atc/commit/77d4a39933bdcd80e250097a1d6862bc7ba32f5d))
* address round-4 PR review comments ([81c4aa0](https://github.com/gitkb/atc/commit/81c4aa0079aa2be64311195f5cc091842b8d5bd4))
* address second round of review comments ([57d2ca8](https://github.com/gitkb/atc/commit/57d2ca854ae38b36f0fc5b901d030c461af95e24))
* **ci:** align with gitkb-core CI patterns ([#24](https://github.com/gitkb/atc/issues/24)) ([8ae0962](https://github.com/gitkb/atc/commit/8ae09621fda9de9429cc028cafceef579498122f))
* **executor:** support non-task dispatches (prompt/template) ([#19](https://github.com/gitkb/atc/issues/19)) ([2d5a91e](https://github.com/gitkb/atc/commit/2d5a91ed04c47a012acb7e1739c489c54fd0dd1b))
* pass --recursive to meta git worktree create ([#46](https://github.com/gitkb/atc/issues/46)) ([fcd37b7](https://github.com/gitkb/atc/commit/fcd37b7f7841729b6bf1901f48d16691aa8fe7ec))
* PR review dispatch broken (template strict mode + worktree naming) ([#23](https://github.com/gitkb/atc/issues/23)) ([8ddbc52](https://github.com/gitkb/atc/commit/8ddbc52587a66e01305fef20dfc5ddf23562de5b))
* remove .claude/prompts/ references, default to .atc/ everywhere ([#40](https://github.com/gitkb/atc/issues/40)) ([f15545e](https://github.com/gitkb/atc/commit/f15545e7a9f3984646bf17b3b9fe270f20adb865))
* resolve {{default_branch}} in templates via provider template_vars ([#39](https://github.com/gitkb/atc/issues/39)) ([fd9916e](https://github.com/gitkb/atc/commit/fd9916e29971fdf589eb8089dca7749bb9419be0))
* resolve_pr_repo_path passes JSON key name instead of value ([#34](https://github.com/gitkb/atc/issues/34)) ([6997974](https://github.com/gitkb/atc/commit/69979746ccd69fa9ce2876aa6118b067268bd520))
* set GITKB_ROOT for all dispatch types, not just task dispatches ([#44](https://github.com/gitkb/atc/issues/44)) ([1bf09fa](https://github.com/gitkb/atc/commit/1bf09fa1f0e0729a46d9a89c38c5c8b418c84b68))
* task resolver respects GITKB_ROOT env var for KB discovery ([#42](https://github.com/gitkb/atc/issues/42)) ([488b4cb](https://github.com/gitkb/atc/commit/488b4cbef0c977d2353a9f04c8d4b2a96bf64451))
* template and directive bugs found during PR review ([#38](https://github.com/gitkb/atc/issues/38)) ([afd845f](https://github.com/gitkb/atc/commit/afd845ff191b123c2518d2a27d9917fc2469113d))
* template frontmatter schema and atc init generation ([#32](https://github.com/gitkb/atc/issues/32)) ([9420407](https://github.com/gitkb/atc/commit/94204072742032a89296ceaf33a2f4cf27b26e3d))
* use git-kb binary name instead of git with kb subarg ([42173f9](https://github.com/gitkb/atc/commit/42173f92916236175840001f7af566bef2bd939c))


### Refactoring

* DispatchOpts/DispatchOutcome structs ([#4](https://github.com/gitkb/atc/issues/4)) ([4f0942c](https://github.com/gitkb/atc/commit/4f0942c4d0ce9c4e1819a20accfcefe1eaded8c1))
* rename Mode to Directive across codebase ([#25](https://github.com/gitkb/atc/issues/25)) ([daa6dba](https://github.com/gitkb/atc/commit/daa6dbae1110ec73f78e63e995c83983274e0fa2))
