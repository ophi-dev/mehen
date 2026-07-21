# Changelog

## [1.6.0](https://github.com/ophi-dev/mehen/compare/v1.5.1...v1.6.0) (2026-07-21)


### chore

* release 1.6.0 ([67952a9](https://github.com/ophi-dev/mehen/commit/67952a92211a446692e218a253b327ddb05a9383))

## [1.5.1](https://github.com/ophi-dev/mehen/compare/v1.5.0...v1.5.1) (2026-07-19)


### Bug Fixes

* import Parser trait in -parser crate doctests + guard doctests in CI ([1e0fb40](https://github.com/ophi-dev/mehen/commit/1e0fb40d30ae6ff0af719280f11705ca16cafb9e))

## [1.5.0](https://github.com/ophi-dev/mehen/compare/v1.4.1...v1.5.0) (2026-07-19)


### Features

* publishable mehen-&lt;lang&gt;-parser crates + antlr-rust-runtime 0.13.0 upgrade ([#189](https://github.com/ophi-dev/mehen/issues/189)) ([af45797](https://github.com/ophi-dev/mehen/commit/af45797779c7ed6100e734516b88d18796939346))

## [1.4.1](https://github.com/ophi-dev/mehen/compare/v1.4.0...v1.4.1) (2026-07-18)


### Parser & Grammar Updates

* bump mago-syntax from 1.42.0 to 1.43.0 in the mago group ([#179](https://github.com/ophi-dev/mehen/issues/179)) ([575f1a1](https://github.com/ophi-dev/mehen/commit/575f1a17c20a0a6c5c16ca386097c61ef7485af6))
* bump ra_ap_syntax from 0.0.341 to 0.0.342 ([#188](https://github.com/ophi-dev/mehen/issues/188)) ([6c5d952](https://github.com/ophi-dev/mehen/commit/6c5d9529bde876c85c0c9c1d52dd9ac1790a6dd8))
* bump the oxc group with 6 updates ([#184](https://github.com/ophi-dev/mehen/issues/184)) ([5a23f0c](https://github.com/ophi-dev/mehen/commit/5a23f0c1e078b353da4ae1ecbd75ec326e86191e))
* bump the ruff group with 3 updates ([#180](https://github.com/ophi-dev/mehen/issues/180)) ([7b6748a](https://github.com/ophi-dev/mehen/commit/7b6748a4b3326315ccea56a9a5408f2dd9e24dea))
* bump tree-sitter from 0.26.10 to 0.26.11 in the tree-sitter group ([#183](https://github.com/ophi-dev/mehen/issues/183)) ([07a8968](https://github.com/ophi-dev/mehen/commit/07a89686aba86ef439037538d4c3e2bf55ff3d44))

## [1.4.0](https://github.com/ophi-dev/mehen/compare/v1.3.0...v1.4.0) (2026-07-10)


### Features

* **metrics:** explain SQL change risk contributions ([#176](https://github.com/ophi-dev/mehen/issues/176)) ([e67f380](https://github.com/ophi-dev/mehen/commit/e67f3805f931f93dcd10fd5a5f4a11b19ef1afc3))

## [1.3.0](https://github.com/ophi-dev/mehen/compare/v1.2.0...v1.3.0) (2026-07-07)


### Features

* add Java analyzer and upgrade antlr-rust-runtime to 0.6.3 ([#160](https://github.com/ophi-dev/mehen/issues/160)) ([3ec7ccb](https://github.com/ophi-dev/mehen/commit/3ec7ccb2724fdc32528ab39a7d80b015bede38e2))

## [1.2.0](https://github.com/ophi-dev/mehen/compare/v1.1.0...v1.2.0) (2026-07-02)


### Features

* **sql:** SQL metrics analyzer (mehen-sql, sqruff-backed) ([#152](https://github.com/ophi-dev/mehen/issues/152)) ([1ff3be5](https://github.com/ophi-dev/mehen/commit/1ff3be549259c36d439b3a01fe5090caf17d4e7b))

## [1.1.0](https://github.com/ophi-dev/mehen/compare/v1.0.2...v1.1.0) (2026-06-22)


### Features

* **kotlin:** replace tree-sitter with ANTLR backend ([#149](https://github.com/ophi-dev/mehen/issues/149)) ([bc0f7fc](https://github.com/ophi-dev/mehen/commit/bc0f7fc1ff827f2dadb387da558661985f801185))

## [1.0.0](https://github.com/ophi-dev/mehen/compare/v1.0.0...v1.0.0) (2026-06-01)


### ⚠ BREAKING CHANGES

* **metrics:** close class-metric gaps, remove --ops/--comments ([#65](https://github.com/ophi-dev/mehen/issues/65))
* **diff:** `mehen diff --metrics mi` (and the action's `metrics:` / `thresholds:` keys referencing `mi`, `maintainability`, or `maintainabilityindex`) no longer resolve — pick an explicit variant (`mi.original`, `mi.sei`, `mi.visual_studio`). Users relying on the default automatically get `mi.visual_studio` now.

### Features

* **action:** retitle comment, add ABC default, auto-exclude tests ([#60](https://github.com/ophi-dev/mehen/issues/60)) ([67225e1](https://github.com/ophi-dev/mehen/commit/67225e1b8eaefae9a46f4d686daf2c0075998b29))
* add `mehen diff` subcommand ([#24](https://github.com/ophi-dev/mehen/issues/24)) ([abcbd57](https://github.com/ophi-dev/mehen/commit/abcbd5777d4326429503e47f407aeef4127cd095))
* add reusable mehen metrics action ([#55](https://github.com/ophi-dev/mehen/issues/55)) ([b30c91a](https://github.com/ophi-dev/mehen/commit/b30c91aa184a6d04c8e8e707c0a59ff26cfd67e3))
* add Ruby language support ([#57](https://github.com/ophi-dev/mehen/issues/57)) ([a88bb87](https://github.com/ophi-dev/mehen/commit/a88bb8753859bacb704e0b46068d69197d6c6ed1))
* align Go metrics with language semantics ([#59](https://github.com/ophi-dev/mehen/issues/59)) ([427393e](https://github.com/ophi-dev/mehen/commit/427393ed2535cff530cd1ef7068f66f64686a54e))
* **cli:** add --version --json and surface version in action footer ([#77](https://github.com/ophi-dev/mehen/issues/77)) ([a233b3f](https://github.com/ophi-dev/mehen/commit/a233b3f5abf49a65aaf0b464c34ecade28528825))
* **cli:** add top-offenders subcommand ([#71](https://github.com/ophi-dev/mehen/issues/71)) ([00e4bf0](https://github.com/ophi-dev/mehen/commit/00e4bf00ab254094c5b5e0a23cff23029b8fd05e))
* **diff:** add Markdown Documentation Metrics section to PR comment (§39) ([#89](https://github.com/ophi-dev/mehen/issues/89)) ([2742bc4](https://github.com/ophi-dev/mehen/commit/2742bc4ffc7d48e0a964c1d699552469652f3451))
* **diff:** split mi into mi.original/mi.sei/mi.visual_studio, default to Visual Studio ([#64](https://github.com/ophi-dev/mehen/issues/64)) ([7c8e4bc](https://github.com/ophi-dev/mehen/commit/7c8e4bc9c7b75bd5ed483017d7b0a598e4be55bc))
* **langs:** add C language support ([#80](https://github.com/ophi-dev/mehen/issues/80)) ([954bb9b](https://github.com/ophi-dev/mehen/commit/954bb9b7da8d16c0733dc7a48ad428b67f16c655))
* **langs:** route JavaScript/JSX through TypeScript/TSX grammars ([#79](https://github.com/ophi-dev/mehen/issues/79)) ([229f37d](https://github.com/ophi-dev/mehen/commit/229f37d0ae732025ddeb17ceeb21c6478c037356))
* **markdown:** add EN+JA prose metric layer (Tier 0, §§29-38) ([#85](https://github.com/ophi-dev/mehen/issues/85)) ([3108e26](https://github.com/ophi-dev/mehen/commit/3108e266cbc3a61470ca6df94214e8b2fd9eb2a7))
* **markdown:** add grounding, evidence, filler risk, RCI, section balance, good scaffold (§§15–21) ([#87](https://github.com/ophi-dev/mehen/issues/87)) ([6fba906](https://github.com/ophi-dev/mehen/commit/6fba9063cebbff4323548086aaee9d24cce1bb01))
* **markdown:** add link debt, visual scaffold, table burden, and artifact debt (§§11–14, §19) ([#84](https://github.com/ophi-dev/mehen/issues/84)) ([8e86186](https://github.com/ophi-dev/mehen/commit/8e861861ee50b077dc35278ea9815b71b9f431cb))
* **markdown:** add MRPC, MCC, Markdown Halstead, and DMI core (§§7–10) ([#83](https://github.com/ophi-dev/mehen/issues/83)) ([9e6eef0](https://github.com/ophi-dev/mehen/commit/9e6eef0a51468a80e5337fa60d73cf11a810c4d6))
* **metrics:** add Kotlin language support ([#66](https://github.com/ophi-dev/mehen/issues/66)) ([f543cef](https://github.com/ophi-dev/mehen/commit/f543cefa1cb0afd1a01a2c7d3a2a07f04f565990))
* **metrics:** add PowerShell language support ([#69](https://github.com/ophi-dev/mehen/issues/69)) ([47b17ac](https://github.com/ophi-dev/mehen/commit/47b17ac4494c9687937b57460be0cda8a1db749e))
* **metrics:** close class-metric gaps, remove --ops/--comments ([#65](https://github.com/ophi-dev/mehen/issues/65)) ([cee0526](https://github.com/ophi-dev/mehen/commit/cee0526b59dc1c46258edae8e8f654749830a727))
* **metrics:** gate wmc/npa/npm by language applicability ([#61](https://github.com/ophi-dev/mehen/issues/61)) ([e4c7cc9](https://github.com/ophi-dev/mehen/commit/e4c7cc959f0ef1f0c1a2878ad4965bd1c674b8c3))
* **php:** add PHP language support ([3cddaf2](https://github.com/ophi-dev/mehen/commit/3cddaf227f7bc674829b5a10145b2de3b72502fd))
* support cargo binstall via GitHub Release archives ([#122](https://github.com/ophi-dev/mehen/issues/122)) ([5f6ff3a](https://github.com/ophi-dev/mehen/commit/5f6ff3a3694dec2711d5886cc0aa3d46a24554ae))


### Bug Fixes

* address PR review comment ([62a7b95](https://github.com/ophi-dev/mehen/commit/62a7b9582fc1ee4cd6bef72823ba03d7d3c29761))
* address PR review comment ([4af0199](https://github.com/ophi-dev/mehen/commit/4af0199ae644200c0fa5623016ad959c84ba1bd1))
* address PR review comment ([e74731a](https://github.com/ophi-dev/mehen/commit/e74731a885eb672eebc00c97eb82a12995db1cc5))
* address PR review comment ([a9d5571](https://github.com/ophi-dev/mehen/commit/a9d5571482a86191ffa3706ba02b4bec6cb5289c))
* address PR review comment ([3dbcfb4](https://github.com/ophi-dev/mehen/commit/3dbcfb48e27a14fd020bdec3f3374886bb93d9a4))
* address PR review comment ([ba68728](https://github.com/ophi-dev/mehen/commit/ba68728fecd66b786372a6aa2922f24d73ad6d19))
* address PR review comment ([1b764e7](https://github.com/ophi-dev/mehen/commit/1b764e73824a16da7d1a333e123bb606d5de4632))
* address PR review comment ([4a6eca8](https://github.com/ophi-dev/mehen/commit/4a6eca8914e971c54dfa82d146f8b3eff837aa63))
* address PR review comments ([12eb0d5](https://github.com/ophi-dev/mehen/commit/12eb0d54c9b6db022858f48201d6fd453296bef2))
* address PR review comments ([f9b0ee3](https://github.com/ophi-dev/mehen/commit/f9b0ee39e94e3624267d943b814a93d3affc9029))
* check PR author instead of event actor in regeneration workflow ([62c82e4](https://github.com/ophi-dev/mehen/commit/62c82e45ee5cea86ae0e460d42ea0b3f4f990aa8))
* **ci:** bump wheel build to manylinux_2_28 for modern libclang ([c5040d2](https://github.com/ophi-dev/mehen/commit/c5040d26a8be9b9604c0e270d62134f8ab13880c))
* **ci:** install libclang in maturin wheel build container ([9645c9f](https://github.com/ophi-dev/mehen/commit/9645c9fa99f9b2f596be0918440e4d4ba5999dfe))
* **ci:** remove broken npm@11 global install from static-analysis ([#48](https://github.com/ophi-dev/mehen/issues/48)) ([7f4cae8](https://github.com/ophi-dev/mehen/commit/7f4cae81b1f018de380d0f98419398489a894e1f))
* handle unmapped Unicode chars in enum generator ([#15](https://github.com/ophi-dev/mehen/issues/15)) ([c972b1d](https://github.com/ophi-dev/mehen/commit/c972b1d451b0cf3bfc3df372213ded864034a056))
* **metrics:** align cyclomatic and cognitive with language semantics ([#63](https://github.com/ophi-dev/mehen/issues/63)) ([e3f436d](https://github.com/ophi-dev/mehen/commit/e3f436d70794ea71d51d415189c723de7f7ab78f))
* **npm:** restore +x on platform binaries and surface spawn errors ([#75](https://github.com/ophi-dev/mehen/issues/75)) ([b5f6104](https://github.com/ophi-dev/mehen/commit/b5f610476db964e999427c797bab4cd04ce9b43c))
* Rust metric edge cases ([#73](https://github.com/ophi-dev/mehen/issues/73)) ([63238d2](https://github.com/ophi-dev/mehen/commit/63238d2729788ee9d7318a3af0ce53973233f1f5))
* trigger on Cargo.lock ([24e216e](https://github.com/ophi-dev/mehen/commit/24e216e0571ab8fff247009ef4fa9d6b67c1c432))
* update README.md ([87ea2b1](https://github.com/ophi-dev/mehen/commit/87ea2b1685d7436bcf9818064047ccb46e4a060c))
* update typos config ([8b8bc9b](https://github.com/ophi-dev/mehen/commit/8b8bc9b3cc485bad5be4696d13bae6f88a64ba4b))
* use GitHub App token in release-please workflow ([#20](https://github.com/ophi-dev/mehen/issues/20)) ([8db91a5](https://github.com/ophi-dev/mehen/commit/8db91a523d36fe949a4ff32e5e650c4a8fae7a09))


### Miscellaneous Chores

* release 0.1.1 ([8f32dba](https://github.com/ophi-dev/mehen/commit/8f32dba7a28775b7cf7f71e2f8c9e1d5fd5dc647))
* release 0.3.0 ([7041e4c](https://github.com/ophi-dev/mehen/commit/7041e4cfdcd33a7e44bbe82d338c1f9f1016b362))
* release 0.4.0 ([8b7a4d3](https://github.com/ophi-dev/mehen/commit/8b7a4d3ec847aa5d003aa5513aa6e893b6ab3851))
* release 0.5.0 ([e0befc2](https://github.com/ophi-dev/mehen/commit/e0befc2e743a3be82f4eca7930146302d7d573d3))
* release 1.0.0 ([285b16a](https://github.com/ophi-dev/mehen/commit/285b16aff747298d1ff3e3085fc2b949850f7ecb))

## [1.0.0](https://github.com/ophidiarium/mehen/compare/v0.7.0...v1.0.0) (2026-06-01)


### ⚠ BREAKING CHANGES

* **metrics:** close class-metric gaps, remove --ops/--comments ([#65](https://github.com/ophidiarium/mehen/issues/65))
* **diff:** `mehen diff --metrics mi` (and the action's `metrics:` / `thresholds:` keys referencing `mi`, `maintainability`, or `maintainabilityindex`) no longer resolve — pick an explicit variant (`mi.original`, `mi.sei`, `mi.visual_studio`). Users relying on the default automatically get `mi.visual_studio` now.

### Features

* **action:** retitle comment, add ABC default, auto-exclude tests ([#60](https://github.com/ophidiarium/mehen/issues/60)) ([67225e1](https://github.com/ophidiarium/mehen/commit/67225e1b8eaefae9a46f4d686daf2c0075998b29))
* add `mehen diff` subcommand ([#24](https://github.com/ophidiarium/mehen/issues/24)) ([abcbd57](https://github.com/ophidiarium/mehen/commit/abcbd5777d4326429503e47f407aeef4127cd095))
* add reusable mehen metrics action ([#55](https://github.com/ophidiarium/mehen/issues/55)) ([b30c91a](https://github.com/ophidiarium/mehen/commit/b30c91aa184a6d04c8e8e707c0a59ff26cfd67e3))
* add Ruby language support ([#57](https://github.com/ophidiarium/mehen/issues/57)) ([a88bb87](https://github.com/ophidiarium/mehen/commit/a88bb8753859bacb704e0b46068d69197d6c6ed1))
* align Go metrics with language semantics ([#59](https://github.com/ophidiarium/mehen/issues/59)) ([427393e](https://github.com/ophidiarium/mehen/commit/427393ed2535cff530cd1ef7068f66f64686a54e))
* **cli:** add --version --json and surface version in action footer ([#77](https://github.com/ophidiarium/mehen/issues/77)) ([a233b3f](https://github.com/ophidiarium/mehen/commit/a233b3f5abf49a65aaf0b464c34ecade28528825))
* **cli:** add top-offenders subcommand ([#71](https://github.com/ophidiarium/mehen/issues/71)) ([00e4bf0](https://github.com/ophidiarium/mehen/commit/00e4bf00ab254094c5b5e0a23cff23029b8fd05e))
* **diff:** add Markdown Documentation Metrics section to PR comment (§39) ([#89](https://github.com/ophidiarium/mehen/issues/89)) ([2742bc4](https://github.com/ophidiarium/mehen/commit/2742bc4ffc7d48e0a964c1d699552469652f3451))
* **diff:** split mi into mi.original/mi.sei/mi.visual_studio, default to Visual Studio ([#64](https://github.com/ophidiarium/mehen/issues/64)) ([7c8e4bc](https://github.com/ophidiarium/mehen/commit/7c8e4bc9c7b75bd5ed483017d7b0a598e4be55bc))
* **langs:** add C language support ([#80](https://github.com/ophidiarium/mehen/issues/80)) ([954bb9b](https://github.com/ophidiarium/mehen/commit/954bb9b7da8d16c0733dc7a48ad428b67f16c655))
* **langs:** route JavaScript/JSX through TypeScript/TSX grammars ([#79](https://github.com/ophidiarium/mehen/issues/79)) ([229f37d](https://github.com/ophidiarium/mehen/commit/229f37d0ae732025ddeb17ceeb21c6478c037356))
* **markdown:** add EN+JA prose metric layer (Tier 0, §§29-38) ([#85](https://github.com/ophidiarium/mehen/issues/85)) ([3108e26](https://github.com/ophidiarium/mehen/commit/3108e266cbc3a61470ca6df94214e8b2fd9eb2a7))
* **markdown:** add grounding, evidence, filler risk, RCI, section balance, good scaffold (§§15–21) ([#87](https://github.com/ophidiarium/mehen/issues/87)) ([6fba906](https://github.com/ophidiarium/mehen/commit/6fba9063cebbff4323548086aaee9d24cce1bb01))
* **markdown:** add link debt, visual scaffold, table burden, and artifact debt (§§11–14, §19) ([#84](https://github.com/ophidiarium/mehen/issues/84)) ([8e86186](https://github.com/ophidiarium/mehen/commit/8e861861ee50b077dc35278ea9815b71b9f431cb))
* **markdown:** add MRPC, MCC, Markdown Halstead, and DMI core (§§7–10) ([#83](https://github.com/ophidiarium/mehen/issues/83)) ([9e6eef0](https://github.com/ophidiarium/mehen/commit/9e6eef0a51468a80e5337fa60d73cf11a810c4d6))
* **metrics:** add Kotlin language support ([#66](https://github.com/ophidiarium/mehen/issues/66)) ([f543cef](https://github.com/ophidiarium/mehen/commit/f543cefa1cb0afd1a01a2c7d3a2a07f04f565990))
* **metrics:** add PowerShell language support ([#69](https://github.com/ophidiarium/mehen/issues/69)) ([47b17ac](https://github.com/ophidiarium/mehen/commit/47b17ac4494c9687937b57460be0cda8a1db749e))
* **metrics:** close class-metric gaps, remove --ops/--comments ([#65](https://github.com/ophidiarium/mehen/issues/65)) ([cee0526](https://github.com/ophidiarium/mehen/commit/cee0526b59dc1c46258edae8e8f654749830a727))
* **metrics:** gate wmc/npa/npm by language applicability ([#61](https://github.com/ophidiarium/mehen/issues/61)) ([e4c7cc9](https://github.com/ophidiarium/mehen/commit/e4c7cc959f0ef1f0c1a2878ad4965bd1c674b8c3))
* **php:** add PHP language support ([3cddaf2](https://github.com/ophidiarium/mehen/commit/3cddaf227f7bc674829b5a10145b2de3b72502fd))
* support cargo binstall via GitHub Release archives ([#122](https://github.com/ophidiarium/mehen/issues/122)) ([5f6ff3a](https://github.com/ophidiarium/mehen/commit/5f6ff3a3694dec2711d5886cc0aa3d46a24554ae))


### Bug Fixes

* address PR review comment ([62a7b95](https://github.com/ophidiarium/mehen/commit/62a7b9582fc1ee4cd6bef72823ba03d7d3c29761))
* address PR review comment ([4af0199](https://github.com/ophidiarium/mehen/commit/4af0199ae644200c0fa5623016ad959c84ba1bd1))
* address PR review comment ([e74731a](https://github.com/ophidiarium/mehen/commit/e74731a885eb672eebc00c97eb82a12995db1cc5))
* address PR review comment ([a9d5571](https://github.com/ophidiarium/mehen/commit/a9d5571482a86191ffa3706ba02b4bec6cb5289c))
* address PR review comment ([3dbcfb4](https://github.com/ophidiarium/mehen/commit/3dbcfb48e27a14fd020bdec3f3374886bb93d9a4))
* address PR review comment ([ba68728](https://github.com/ophidiarium/mehen/commit/ba68728fecd66b786372a6aa2922f24d73ad6d19))
* address PR review comment ([1b764e7](https://github.com/ophidiarium/mehen/commit/1b764e73824a16da7d1a333e123bb606d5de4632))
* address PR review comment ([4a6eca8](https://github.com/ophidiarium/mehen/commit/4a6eca8914e971c54dfa82d146f8b3eff837aa63))
* address PR review comments ([12eb0d5](https://github.com/ophidiarium/mehen/commit/12eb0d54c9b6db022858f48201d6fd453296bef2))
* address PR review comments ([f9b0ee3](https://github.com/ophidiarium/mehen/commit/f9b0ee39e94e3624267d943b814a93d3affc9029))
* check PR author instead of event actor in regeneration workflow ([62c82e4](https://github.com/ophidiarium/mehen/commit/62c82e45ee5cea86ae0e460d42ea0b3f4f990aa8))
* **ci:** remove broken npm@11 global install from static-analysis ([#48](https://github.com/ophidiarium/mehen/issues/48)) ([7f4cae8](https://github.com/ophidiarium/mehen/commit/7f4cae81b1f018de380d0f98419398489a894e1f))
* handle unmapped Unicode chars in enum generator ([#15](https://github.com/ophidiarium/mehen/issues/15)) ([c972b1d](https://github.com/ophidiarium/mehen/commit/c972b1d451b0cf3bfc3df372213ded864034a056))
* **metrics:** align cyclomatic and cognitive with language semantics ([#63](https://github.com/ophidiarium/mehen/issues/63)) ([e3f436d](https://github.com/ophidiarium/mehen/commit/e3f436d70794ea71d51d415189c723de7f7ab78f))
* **npm:** restore +x on platform binaries and surface spawn errors ([#75](https://github.com/ophidiarium/mehen/issues/75)) ([b5f6104](https://github.com/ophidiarium/mehen/commit/b5f610476db964e999427c797bab4cd04ce9b43c))
* Rust metric edge cases ([#73](https://github.com/ophidiarium/mehen/issues/73)) ([63238d2](https://github.com/ophidiarium/mehen/commit/63238d2729788ee9d7318a3af0ce53973233f1f5))
* trigger on Cargo.lock ([24e216e](https://github.com/ophidiarium/mehen/commit/24e216e0571ab8fff247009ef4fa9d6b67c1c432))
* update README.md ([87ea2b1](https://github.com/ophidiarium/mehen/commit/87ea2b1685d7436bcf9818064047ccb46e4a060c))
* update typos config ([8b8bc9b](https://github.com/ophidiarium/mehen/commit/8b8bc9b3cc485bad5be4696d13bae6f88a64ba4b))
* use GitHub App token in release-please workflow ([#20](https://github.com/ophidiarium/mehen/issues/20)) ([8db91a5](https://github.com/ophidiarium/mehen/commit/8db91a523d36fe949a4ff32e5e650c4a8fae7a09))


### Miscellaneous Chores

* release 0.1.1 ([8f32dba](https://github.com/ophidiarium/mehen/commit/8f32dba7a28775b7cf7f71e2f8c9e1d5fd5dc647))
* release 0.3.0 ([7041e4c](https://github.com/ophidiarium/mehen/commit/7041e4cfdcd33a7e44bbe82d338c1f9f1016b362))
* release 0.4.0 ([8b7a4d3](https://github.com/ophidiarium/mehen/commit/8b7a4d3ec847aa5d003aa5513aa6e893b6ab3851))
* release 0.5.0 ([e0befc2](https://github.com/ophidiarium/mehen/commit/e0befc2e743a3be82f4eca7930146302d7d573d3))
* release 1.0.0 ([285b16a](https://github.com/ophidiarium/mehen/commit/285b16aff747298d1ff3e3085fc2b949850f7ecb))

## [0.7.0](https://github.com/ophidiarium/mehen/compare/v0.6.1...v0.7.0) (2026-05-18)


### Features

* **php:** add PHP language support ([4b42b9e](https://github.com/ophidiarium/mehen/commit/4b42b9ef39a9255d932737a0bc597139da8b603f))


### Bug Fixes

* address PR review comment ([d29c594](https://github.com/ophidiarium/mehen/commit/d29c594f6520a05713b18ead105ccc3d692a15c9))
* address PR review comment ([6f04185](https://github.com/ophidiarium/mehen/commit/6f0418513cb5dcdf753e40cdf903f05c77a098c9))
* address PR review comment ([580d09b](https://github.com/ophidiarium/mehen/commit/580d09b143d8b26076aea1bd83ed3612213ac13d))
* address PR review comment ([64cec1e](https://github.com/ophidiarium/mehen/commit/64cec1e761afcf849a88ea6f4e20623c2e82db2c))
* address PR review comment ([b8d773a](https://github.com/ophidiarium/mehen/commit/b8d773a914187e8c64436e1f00bd479fd28ceae6))
* address PR review comment ([2f84abf](https://github.com/ophidiarium/mehen/commit/2f84abf5c50b0dfe01b0958e5515398d9f84a841))
* address PR review comment ([08435b5](https://github.com/ophidiarium/mehen/commit/08435b5ab050eab4c89a9207031150f1f4af9fd1))
* address PR review comment ([06d79f5](https://github.com/ophidiarium/mehen/commit/06d79f57b61f0e0a4faa736644978082dc3bd0f8))
* address PR review comments ([5d0559d](https://github.com/ophidiarium/mehen/commit/5d0559d5d453dd21fd11bc8ce2a33c9eba8b5e95))
* address PR review comments ([56028e7](https://github.com/ophidiarium/mehen/commit/56028e7dede525b661db88dd8ab16a0c6a65330f))

## [0.6.1](https://github.com/ophidiarium/mehen/compare/v0.6.0...v0.6.1) (2026-05-15)


### Bug Fixes

* update README.md ([8fe00df](https://github.com/ophidiarium/mehen/commit/8fe00dfd2844ea153b06eb3765fdd1cca2844b5b))

## [0.6.0](https://github.com/ophidiarium/mehen/compare/v0.5.0...v0.6.0) (2026-05-13)


### Features

* **diff:** add Markdown Documentation Metrics section to PR comment (§39) ([#89](https://github.com/ophidiarium/mehen/issues/89)) ([1563333](https://github.com/ophidiarium/mehen/commit/15633339e9c4b3f68a0513682a68d6fb813cc611))
* **markdown:** add EN+JA prose metric layer (Tier 0, §§29-38) ([#85](https://github.com/ophidiarium/mehen/issues/85)) ([da64470](https://github.com/ophidiarium/mehen/commit/da644705c5d006ee902601505a2dd5437acc9953))
* **markdown:** add grounding, evidence, filler risk, RCI, section balance, good scaffold (§§15–21) ([#87](https://github.com/ophidiarium/mehen/issues/87)) ([a948fbc](https://github.com/ophidiarium/mehen/commit/a948fbc43b136becfe5c5fbd5bd36a93d3ecb89a))
* **markdown:** add link debt, visual scaffold, table burden, and artifact debt (§§11–14, §19) ([#84](https://github.com/ophidiarium/mehen/issues/84)) ([9e23a0a](https://github.com/ophidiarium/mehen/commit/9e23a0a93a50759cd0bbdd5a52920e46b01b2893))
* **markdown:** add MRPC, MCC, Markdown Halstead, and DMI core (§§7–10) ([#83](https://github.com/ophidiarium/mehen/issues/83)) ([2fdd4d1](https://github.com/ophidiarium/mehen/commit/2fdd4d11ac7458eb1a9d6b6ba45aa7cba0e5c9ea))

## [0.5.0](https://github.com/ophidiarium/mehen/compare/v0.4.3...v0.5.0) (2026-05-11)


### Features

* **cli:** add --version --json and surface version in action footer ([#77](https://github.com/ophidiarium/mehen/issues/77)) ([199e37d](https://github.com/ophidiarium/mehen/commit/199e37d31bfb1fec1746f78f484330b0056614ea))
* **langs:** add C language support ([#80](https://github.com/ophidiarium/mehen/issues/80)) ([783a1e8](https://github.com/ophidiarium/mehen/commit/783a1e886cf43907f8370693f6bf09e859dcc508))
* **langs:** route JavaScript/JSX through TypeScript/TSX grammars ([#79](https://github.com/ophidiarium/mehen/issues/79)) ([a6fb345](https://github.com/ophidiarium/mehen/commit/a6fb345898ea965bfb7f77983c774d6558b842a6))


### Miscellaneous Chores

* release 0.5.0 ([b9f81c3](https://github.com/ophidiarium/mehen/commit/b9f81c3ee5223736d72f21fbf933724e86000945))

## [0.4.3](https://github.com/ophidiarium/mehen/compare/v0.4.2...v0.4.3) (2026-05-09)


### Bug Fixes

* **npm:** restore +x on platform binaries and surface spawn errors ([#75](https://github.com/ophidiarium/mehen/issues/75)) ([e12549e](https://github.com/ophidiarium/mehen/commit/e12549eb9bcccb586f424101a5d1ee50a41aec6d))

## [0.4.2](https://github.com/ophidiarium/mehen/compare/v0.4.1...v0.4.2) (2026-05-08)


### Bug Fixes

* Rust metric edge cases ([#73](https://github.com/ophidiarium/mehen/issues/73)) ([8bccab4](https://github.com/ophidiarium/mehen/commit/8bccab409c6e8cac29e737b085baebbf4ba14d61))

## [0.4.1](https://github.com/ophidiarium/mehen/compare/v0.4.0...v0.4.1) (2026-05-07)


### Features

* **cli:** add top-offenders subcommand ([#71](https://github.com/ophidiarium/mehen/issues/71)) ([e07c061](https://github.com/ophidiarium/mehen/commit/e07c061463ed55fa5736b9e71f09979eba1d39a4))

## [0.4.0](https://github.com/ophidiarium/mehen/compare/v0.3.0...v0.4.0) (2026-05-05)


### Features

* **metrics:** add PowerShell language support ([#69](https://github.com/ophidiarium/mehen/issues/69)) ([7a34436](https://github.com/ophidiarium/mehen/commit/7a344361c086214f0b98730e6bae10f02b0ab22a))


### Miscellaneous Chores

* release 0.4.0 ([5a3a699](https://github.com/ophidiarium/mehen/commit/5a3a69986f6193a17bfbbed0831d8c648075c0b7))

## [0.3.0](https://github.com/ophidiarium/mehen/compare/v0.2.0...v0.3.0) (2026-05-02)


### Features

* **metrics:** add Kotlin language support ([#66](https://github.com/ophidiarium/mehen/issues/66)) ([028f93b](https://github.com/ophidiarium/mehen/commit/028f93b802241d742e16d4952d798dc980a8535a))


### Miscellaneous Chores

* release 0.3.0 ([8b6dad6](https://github.com/ophidiarium/mehen/commit/8b6dad68dc3668a55666e46498b539dff073dd4f))

## [0.2.0](https://github.com/ophidiarium/mehen/compare/v0.1.1...v0.2.0) (2026-04-30)


### ⚠ BREAKING CHANGES

* **metrics:** close class-metric gaps, remove --ops/--comments ([#65](https://github.com/ophidiarium/mehen/issues/65))
* **diff:** `mehen diff --metrics mi` (and the action's `metrics:` / `thresholds:` keys referencing `mi`, `maintainability`, or `maintainabilityindex`) no longer resolve — pick an explicit variant (`mi.original`, `mi.sei`, `mi.visual_studio`). Users relying on the default automatically get `mi.visual_studio` now.

### Features

* **diff:** split mi into mi.original/mi.sei/mi.visual_studio, default to Visual Studio ([#64](https://github.com/ophidiarium/mehen/issues/64)) ([cb22ed6](https://github.com/ophidiarium/mehen/commit/cb22ed6afd0def04b0f8b9336a95389469fcf361))
* **metrics:** close class-metric gaps, remove --ops/--comments ([#65](https://github.com/ophidiarium/mehen/issues/65)) ([25a8218](https://github.com/ophidiarium/mehen/commit/25a8218f1c67c7d620f19ddc6f7fb70d7e00c7ab))
* **metrics:** gate wmc/npa/npm by language applicability ([#61](https://github.com/ophidiarium/mehen/issues/61)) ([975726a](https://github.com/ophidiarium/mehen/commit/975726a87c9ee112da7c4a2385ac687c805abd6a))


### Bug Fixes

* **metrics:** align cyclomatic and cognitive with language semantics ([#63](https://github.com/ophidiarium/mehen/issues/63)) ([7425460](https://github.com/ophidiarium/mehen/commit/7425460f91c87c360dee2443878d925b2bce0b4f))

## [0.1.1](https://github.com/ophidiarium/mehen/compare/v0.0.6...v0.1.1) (2026-04-30)


### Features

* **action:** retitle comment, add ABC default, auto-exclude tests ([#60](https://github.com/ophidiarium/mehen/issues/60)) ([fb470d6](https://github.com/ophidiarium/mehen/commit/fb470d6772c5e46e88ead129e14871ca54afd944))
* add Ruby language support ([#57](https://github.com/ophidiarium/mehen/issues/57)) ([fb8d53c](https://github.com/ophidiarium/mehen/commit/fb8d53cca1d88a916a15e0d2e9e9297df2f8c145))
* align Go metrics with language semantics ([#59](https://github.com/ophidiarium/mehen/issues/59)) ([ae3cf53](https://github.com/ophidiarium/mehen/commit/ae3cf53684e78e717457c9edee9b4eaa29640587))


### Miscellaneous Chores

* release 0.1.1 ([03018d7](https://github.com/ophidiarium/mehen/commit/03018d76f4978f8e219b93b0c2f625c3f7b922d7))

## [0.0.6](https://github.com/ophidiarium/mehen/compare/v0.0.5...v0.0.6) (2026-04-25)


### Features

* add reusable mehen metrics action ([#55](https://github.com/ophidiarium/mehen/issues/55)) ([11c5a52](https://github.com/ophidiarium/mehen/commit/11c5a529e4d006fa74f15cd453fb188c36ab9c40))

## [0.0.5](https://github.com/ophidiarium/mehen/compare/v0.0.4...v0.0.5) (2026-04-07)


### Bug Fixes

* **ci:** remove broken npm@11 global install from static-analysis ([#48](https://github.com/ophidiarium/mehen/issues/48)) ([1774559](https://github.com/ophidiarium/mehen/commit/17745598d764638d2736e2ca539872ff7666c42b))

## [0.0.4](https://github.com/ophidiarium/mehen/compare/v0.0.3...v0.0.4) (2026-02-16)


### Features

* add `mehen diff` subcommand ([#24](https://github.com/ophidiarium/mehen/issues/24)) ([8edfcdf](https://github.com/ophidiarium/mehen/commit/8edfcdfef6b98c726becc681533bbb6a89c237db))

## [0.0.3](https://github.com/ophidiarium/mehen/compare/v0.0.2...v0.0.3) (2026-02-16)


### Bug Fixes

* update typos config ([a41a4a4](https://github.com/ophidiarium/mehen/commit/a41a4a462e96d965641a4615e2403646c4e0885a))
* use GitHub App token in release-please workflow ([#20](https://github.com/ophidiarium/mehen/issues/20)) ([88cf7ba](https://github.com/ophidiarium/mehen/commit/88cf7ba030b4ffbe74203cef9250e7d56702184b))

## [0.0.2](https://github.com/ophidiarium/mehen/compare/v0.0.1...v0.0.2) (2026-02-15)


### Features

* Add Go language support ([4ed54eb](https://github.com/ophidiarium/mehen/commit/4ed54ebbebde67ae6429c1ef31349d8a2b866dd4))
* Expose inner tree-sitter::Node for advanced use cases ([#1210](https://github.com/ophidiarium/mehen/issues/1210)) ([2e167b0](https://github.com/ophidiarium/mehen/commit/2e167b00f906629b2341450e4485b570f8c02f0e))


### Bug Fixes

* Address clippy collapsible_if warnings with let-chains ([#1211](https://github.com/ophidiarium/mehen/issues/1211)) ([383c639](https://github.com/ophidiarium/mehen/commit/383c639cfe3ca6aa84f3c73e1df68beaff7151c7))
* check PR author instead of event actor in regeneration workflow ([9c91fcc](https://github.com/ophidiarium/mehen/commit/9c91fcce6eaef2bb212e3146d70cb66f8ea8a080))
* handle unmapped Unicode chars in enum generator ([#15](https://github.com/ophidiarium/mehen/issues/15)) ([1f3e115](https://github.com/ophidiarium/mehen/commit/1f3e115dba83f4e2d488718a6d2e0fea82d830d6))
* hanging server ([#806](https://github.com/ophidiarium/mehen/issues/806)) ([08c61f4](https://github.com/ophidiarium/mehen/commit/08c61f48ad7ea3ad20bff9deb64fd211e7f93857))
* trigger on Cargo.lock ([3ec887b](https://github.com/ophidiarium/mehen/commit/3ec887b89732c64004fcc97191728fe5de0ae48a))
