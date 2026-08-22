# Changelog

## [0.1.1](https://github.com/TheOrdinaryWow/oc-clean/compare/v0.1.0...v0.1.1) (2026-08-22)


### Bug Fixes

* **ci:** build musl targets natively instead of through cross ([2a63acc](https://github.com/TheOrdinaryWow/oc-clean/commit/2a63acc2e5b6d953d2451e9c74b92b69d5a6b055))
* **ci:** name the release branch after the component so releases get tagged ([a60b200](https://github.com/TheOrdinaryWow/oc-clean/commit/a60b200ea814d759a65b7205238605eb88ad94bc))

## 0.1.0 (2026-08-22)


### Features

* adapt rust-skills for opencode ([c2f698d](https://github.com/TheOrdinaryWow/oc-clean/commit/c2f698d2b58cf8a60006a1369467acef65d8bbe5))
* **analyze:** add age distribution and external directory overview ([07040b5](https://github.com/TheOrdinaryWow/oc-clean/commit/07040b5489cc12b1a1ab75764d11ef37d4526d7d))
* **analyze:** add analyze command with human and JSON output ([2d1ed9e](https://github.com/TheOrdinaryWow/oc-clean/commit/2d1ed9e986ff7da70d2a8fe967b68aa5cb9fadc5))
* **analyze:** add file and per-table space accounting ([c0e0d3b](https://github.com/TheOrdinaryWow/oc-clean/commit/c0e0d3b2b9dd284a11b8808c35c98f7dfc8a615f))
* **analyze:** add orphan census across all orphan classes ([9299af4](https://github.com/TheOrdinaryWow/oc-clean/commit/9299af4ac28ef5f1e33cd13abf489601a96a9b00))
* **analyze:** add per-project and per-session size attribution ([b90dafa](https://github.com/TheOrdinaryWow/oc-clean/commit/b90dafa8bb10b9b3701fff2e2ba0658d7c9d8819))
* **analyze:** put the technical layers behind --detailed and reorder the report ([e5ef8b2](https://github.com/TheOrdinaryWow/oc-clean/commit/e5ef8b2596a9f7e818f88b9f34bfd4881883545e))
* **analyze:** report session titles, activity dates, and message counts ([6bee456](https://github.com/TheOrdinaryWow/oc-clean/commit/6bee45616f75b4ec395bb4949284068ef4c45386))
* **assets:** add opt-in snapshot repository compaction ([e9f9b91](https://github.com/TheOrdinaryWow/oc-clean/commit/e9f9b915d4d36f9eeddf7f2b0375667d853aabc0))
* **assets:** remove snapshot directories for pruned projects ([a7b2996](https://github.com/TheOrdinaryWow/oc-clean/commit/a7b2996eb18008646e4dbc0606fdc3df88f7380f))
* **assets:** sweep orphaned storage files ([7c2ad70](https://github.com/TheOrdinaryWow/oc-clean/commit/7c2ad70b7b73de02c8e716596e5f2a24c51ae838))
* **clean:** wire clean command with graceful interrupt ([6dbd158](https://github.com/TheOrdinaryWow/oc-clean/commit/6dbd1580eaefaaa406700103a58d707769d5d4d0))
* **cli:** accept memory db and channel override ([351fe74](https://github.com/TheOrdinaryWow/oc-clean/commit/351fe7490301d2aebc3ef358de3fdc6d1e7bf948))
* **cli:** accept the usual boolean spellings in environment switches ([877040a](https://github.com/TheOrdinaryWow/oc-clean/commit/877040a0ae2629bc7e594ca1512b01ad33dc3b32))
* **cli:** add coarse duration and SI size argument types ([7bfb591](https://github.com/TheOrdinaryWow/oc-clean/commit/7bfb59179336c0656d078de3dc76c37659c069fa))
* **cli:** confirm destructive runs interactively instead of requiring a flag ([2a1dbd6](https://github.com/TheOrdinaryWow/oc-clean/commit/2a1dbd62cfae71d0662ea4469e81bb5cabf325a7))
* **cli:** replace --log-format with global --log and default logging to off ([39f6262](https://github.com/TheOrdinaryWow/oc-clean/commit/39f6262bb944bc46fafc2047009888630a75baad))
* **cli:** replace --project with mutually exclusive --include and --exclude ([102eaa6](https://github.com/TheOrdinaryWow/oc-clean/commit/102eaa6ec867e885920f0409c43ec669d7397d52))
* **db:** add connection layer with PRAGMAs and capability probes ([ad3b49f](https://github.com/TheOrdinaryWow/oc-clean/commit/ad3b49f6a32fb361f7e62d79afceb6388d3b761e))
* **db:** add three-tier schema compatibility check ([ab5b995](https://github.com/TheOrdinaryWow/oc-clean/commit/ab5b995092df6067db2b9962b59ac936d8ee3df8))
* **delete:** add batched session deletion with event cleanup ([3a26950](https://github.com/TheOrdinaryWow/oc-clean/commit/3a269508d5eae23e449c4a8aa6da46eff4a821cf))
* **delete:** add pre-existing orphan sweep ([2238b54](https://github.com/TheOrdinaryWow/oc-clean/commit/2238b5406aefc7194cb422c5c56d784ef5269a18))
* **delete:** prune projects left with no sessions ([2827243](https://github.com/TheOrdinaryWow/oc-clean/commit/2827243e1dad76b8cdd43bca6f0a778e51173109))
* **doctor:** add health check command ([0b5f4e8](https://github.com/TheOrdinaryWow/oc-clean/commit/0b5f4e884cad232e14f5177459c6cd4176eee96d))
* **error:** add typed error taxonomy with stable exit codes ([2ca2649](https://github.com/TheOrdinaryWow/oc-clean/commit/2ca2649d27451367391db23ce8404da7c3074525))
* init cargo workspace ([1d96492](https://github.com/TheOrdinaryWow/oc-clean/commit/1d96492f81cbaa2fe215d5acd62207db615a3112))
* **paths:** resolve OpenCode data dir and database path ([4a7e5d0](https://github.com/TheOrdinaryWow/oc-clean/commit/4a7e5d097ca987ecf35f7dbad71441499e51dcad))
* **progress:** add bounded parallel execution and richer bar decorations ([b863063](https://github.com/TheOrdinaryWow/oc-clean/commit/b863063cf3e7285500cec61c84f28551c024ee68))
* **progress:** render real progress bars for analyze, clean, doctor, and vacuum ([3412de2](https://github.com/TheOrdinaryWow/oc-clean/commit/3412de23f015ebe016e4ba7bec7b59f294c33d38))
* **reclaim:** add disk headroom pre-check ([02a7732](https://github.com/TheOrdinaryWow/oc-clean/commit/02a7732ad3fb2cb4ca097b7486dfc7364ab6aac6))
* **reclaim:** add incremental vacuum alternative ([d4e6fd4](https://github.com/TheOrdinaryWow/oc-clean/commit/d4e6fd460a77e00debc66007a34e553a634fef3f))
* **reclaim:** add VACUUM INTO with verification and atomic swap ([dd4466f](https://github.com/TheOrdinaryWow/oc-clean/commit/dd4466faf02c9deb92e013126fefad1c84ce803e))
* **report:** add impact summary and dry-run rendering ([e1cd5cb](https://github.com/TheOrdinaryWow/oc-clean/commit/e1cd5cbdeeffe5716718a9b007a2e4faea99a30a))
* **report:** align tables with comfy-table and color output with owo-colors ([7eb8f76](https://github.com/TheOrdinaryWow/oc-clean/commit/7eb8f76eca985e529227ba3c5f7ea03c19a36df8))
* **report:** key the project rollup by worktree path instead of identifier ([958000a](https://github.com/TheOrdinaryWow/oc-clean/commit/958000a748c7b3086f9ca156af009661556377dd))
* **report:** show the owning project path for every listed session ([c3d773b](https://github.com/TheOrdinaryWow/oc-clean/commit/c3d773b19342f241e31f8ab979e380bfa30edd79))
* **safety:** add confirmation contract and progress plumbing ([72c814b](https://github.com/TheOrdinaryWow/oc-clean/commit/72c814b4d9ad92e45ee2d337444c2c1ffa153c80))
* **safety:** add cross-platform process holder detection ([c3a8ab8](https://github.com/TheOrdinaryWow/oc-clean/commit/c3a8ab8bf5168fae1ee6f4b0342cf92c1e37f9cc))
* **select:** add age, archived, and project predicates ([6da89be](https://github.com/TheOrdinaryWow/oc-clean/commit/6da89be6fa067d993c69349098017a419b86d11c))
* **select:** add orphan detection for the deletion path ([dcf12ab](https://github.com/TheOrdinaryWow/oc-clean/commit/dcf12ab3e6db3d945af439b3f4e6ac41c060890b))
* **select:** add per-project keep-recent retention ([a1b8645](https://github.com/TheOrdinaryWow/oc-clean/commit/a1b8645cfbb4dcb1c8a5ffbc8c7e2aee3be2963b))
* **select:** add subtree expansion and size predicate ([b4ffe07](https://github.com/TheOrdinaryWow/oc-clean/commit/b4ffe07785c5c2f16be32299b26786cb8fc85f9f))
* **skill:** add rust-skills ([71a6312](https://github.com/TheOrdinaryWow/oc-clean/commit/71a6312a362bc69f828ba9f60f8a3ec3a0cf86f0))
* **vacuum:** add vacuum command with headroom handling ([c5f06e7](https://github.com/TheOrdinaryWow/oc-clean/commit/c5f06e78ab02d128877a6ea0fd8e25848716c503))


### Bug Fixes

* **analyze:** include context-epoch bytes in age distribution ([0e940f6](https://github.com/TheOrdinaryWow/oc-clean/commit/0e940f6ed104acf7bf5123d00f7e7a81bc8e9800))
* **analyze:** keep in-memory derived paths valid on every platform ([2db6473](https://github.com/TheOrdinaryWow/oc-clean/commit/2db64734b8cf764b7f2b6cb484150608bcfd71d8))
* **analyze:** treat malformed sibling paths as absent directories ([9f73463](https://github.com/TheOrdinaryWow/oc-clean/commit/9f73463c7f5f2fb6b83dbd2052b02c60ec407256))
* **assets:** anchor snapshot gc child process with fchdir ([a5b1f43](https://github.com/TheOrdinaryWow/oc-clean/commit/a5b1f435d6c7fa4e2d43b530f487449902672279))
* **assets:** anchor storage and snapshot GC traversal to opened handles ([d0a6143](https://github.com/TheOrdinaryWow/oc-clean/commit/d0a61432af8773dd54da9ae495d9bc43e9d50463))
* **assets:** prevent symlink escape during snapshot delete ([8aa1d7a](https://github.com/TheOrdinaryWow/oc-clean/commit/8aa1d7a78c4f794ff44cddadadd102ec4f837f5a))
* **clean:** align cleanup step contracts ([514795e](https://github.com/TheOrdinaryWow/oc-clean/commit/514795e0a8c0b5a06578d5b370ebbf235cb05624))
* **clean:** align impact progress timing ([3bbcf5a](https://github.com/TheOrdinaryWow/oc-clean/commit/3bbcf5a99fb5aebf5203c242f484f485e8ba4943))
* **clean:** bound WAL dirty-page footprint and classify lock errors ([428741a](https://github.com/TheOrdinaryWow/oc-clean/commit/428741a2dfca3f107dee0c9c5ba2dcd905623fd5))
* **clean:** correct step labels and headroom timing ([727efa0](https://github.com/TheOrdinaryWow/oc-clean/commit/727efa02b88a35621b6f8ee37ce15275cd4f7ba9))
* **clean:** keep progress bars off the screen while reports and prompts are written ([bcac431](https://github.com/TheOrdinaryWow/oc-clean/commit/bcac431abae0161c54ca6bc20b697d474ddbf33f))
* **clean:** treat macOS project paths as case-insensitive ([ff2cd18](https://github.com/TheOrdinaryWow/oc-clean/commit/ff2cd1837e6b7191a4b1dbd08e9b8353cf9cd03e))
* **cli:** complete cleanup option wiring ([4c8ed51](https://github.com/TheOrdinaryWow/oc-clean/commit/4c8ed513beabdc50e55b332e1969a5b97bc28da7))
* **db:** classify SQLite busy/locked in remaining error paths ([38f376b](https://github.com/TheOrdinaryWow/oc-clean/commit/38f376bafda07fced1cfc4f8f27a4b6cf05b1aca))
* **db:** classify symlink cycles as resolution errors instead of missing files ([60a0198](https://github.com/TheOrdinaryWow/oc-clean/commit/60a019853bed8b7853e6f67b80d95c2977dfe2ae))
* **db:** derive file identity from open handles ([0cc4baa](https://github.com/TheOrdinaryWow/oc-clean/commit/0cc4baad1e5b584caabf2d1889e42b7ff9cff13d))
* **db:** drop the unused descriptor path helper ([04cd66f](https://github.com/TheOrdinaryWow/oc-clean/commit/04cd66ffdcae90deff69f19e83050f125e400e9d))
* **db:** link backups from the anchored parent directory ([d8fc3d2](https://github.com/TheOrdinaryWow/oc-clean/commit/d8fc3d2ecd3fec20fa5cab79341d5b6604c1961c))
* **db:** resolve database siblings by path off linux ([10ce6fc](https://github.com/TheOrdinaryWow/oc-clean/commit/10ce6fc6e1d9f1b4931601af80b35cec095eb03f))
* **db:** share delete access on the windows database anchor ([6b50af5](https://github.com/TheOrdinaryWow/oc-clean/commit/6b50af57caa38d0f5eb0f29a9bb6779ae2c41a68))
* **db:** stop holding a database handle off unix ([0aeebf4](https://github.com/TheOrdinaryWow/oc-clean/commit/0aeebf4967d563210e85057d54dc346def2823ae))
* **db:** use path based database access off linux ([2324683](https://github.com/TheOrdinaryWow/oc-clean/commit/2324683df0fed951afe72fb35de27a99c69febe9))
* **delete:** classify lock contention as busy ([6c1dfd9](https://github.com/TheOrdinaryWow/oc-clean/commit/6c1dfd943ede7bb3e3933edfc596c339172c46c4))
* **delete:** distinguish orphan event aggregates ([1db2fbb](https://github.com/TheOrdinaryWow/oc-clean/commit/1db2fbb2d34d0725b22d4097c5eceb0f8a93bf71))
* **delete:** stop orphan sweep after interrupt ([3213535](https://github.com/TheOrdinaryWow/oc-clean/commit/32135353304a6d80dafff5892569d234dc8dfecd))
* **doctor:** align health check failure contracts ([e0de4d5](https://github.com/TheOrdinaryWow/oc-clean/commit/e0de4d55ac5993d58caf358101111058b4a0f73c))
* **doctor:** share the in-memory placeholder data directory ([84056ee](https://github.com/TheOrdinaryWow/oc-clean/commit/84056ee0c35d0dcf2af2a59acefbf9cbf441d475))
* **error:** classify SQLite lock contention consistently across analyze, report, and doctor ([4b4cae1](https://github.com/TheOrdinaryWow/oc-clean/commit/4b4cae1387f34ad1357ff1af91cc05fc62d7e3c5))
* **reclaim:** bound skewed batch WAL allowance ([4fef6b6](https://github.com/TheOrdinaryWow/oc-clean/commit/4fef6b661f6b37a9e61e3d0a285937f7b2fbfb76))
* **reclaim:** budget the full delete batch WAL ([6919047](https://github.com/TheOrdinaryWow/oc-clean/commit/6919047eb218fb70ce95f494d77595a808751ded))
* **reclaim:** classify windows swap contention as busy ([8ec749c](https://github.com/TheOrdinaryWow/oc-clean/commit/8ec749c4bf0f52e243406b8702b2af850f2c07ff))
* **reclaim:** flush the swapped database through a writable handle ([2941e59](https://github.com/TheOrdinaryWow/oc-clean/commit/2941e59b4ed77f1bef43a54cf54c49db26d09880))
* **reclaim:** harden backup copy creation ([1c9eeeb](https://github.com/TheOrdinaryWow/oc-clean/commit/1c9eeeb5a611617b79dedc80c4a497d5612d3596))
* **reclaim:** map incremental vacuum lock errors to DatabaseBusy ([96bf3cb](https://github.com/TheOrdinaryWow/oc-clean/commit/96bf3cb14c2537d4645ae4314bbad0d0bd5d41e8))
* **reclaim:** skip backup-copy headroom budget under --skip-backup ([5e3213f](https://github.com/TheOrdinaryWow/oc-clean/commit/5e3213f0e22c57ba46283c1fe17180ac816eb875))
* **reclaim:** skip directory flush on windows ([e2a132e](https://github.com/TheOrdinaryWow/oc-clean/commit/e2a132ee0f7d793fde6929b2744664adf568b779))
* **reclaim:** treat denied access as a busy database on windows ([bf586c2](https://github.com/TheOrdinaryWow/oc-clean/commit/bf586c2caa95e58ae8e55aec97a88064b4642b36))
* **report:** apply prune-empty-projects scope to the dry-run preview ([c32d896](https://github.com/TheOrdinaryWow/oc-clean/commit/c32d896f195942436bff20cb366531291c2a1020))
* **safety:** align symlink holder and swap targets ([ec9715f](https://github.com/TheOrdinaryWow/oc-clean/commit/ec9715fc0ca74db516db4d03ca36ac3d6e6547e5))
* **safety:** allow restart manager unsafe blocks under the crate lint ([6d5a7b2](https://github.com/TheOrdinaryWow/oc-clean/commit/6d5a7b23320e8fe8f510dbd7031b84509a94fab0))
* **safety:** anchor database path identity ([0f329a8](https://github.com/TheOrdinaryWow/oc-clean/commit/0f329a89f1714a7d7981bd5fb175694dc22eca9a))
* **safety:** pin database filesystem operations ([27f0f99](https://github.com/TheOrdinaryWow/oc-clean/commit/27f0f991a1561871de2651521d1ff47c40809a6e))
* **safety:** preserve lexical holder paths ([377df03](https://github.com/TheOrdinaryWow/oc-clean/commit/377df03c395954712386dd937cab9c1dbbf065a7))
* **safety:** resolve multi-hop database symlinks ([68a438b](https://github.com/TheOrdinaryWow/oc-clean/commit/68a438bba412e15378525bbd06ab9bad4fee3cfa))
* **safety:** warn instead of refusing when the holder scan is permission limited ([c522590](https://github.com/TheOrdinaryWow/oc-clean/commit/c522590dbbb1197af2d124e82f262aba41807a1d))
* **select:** chunk subtree expansion IN queries below SQLite's variable limit ([285b70d](https://github.com/TheOrdinaryWow/oc-clean/commit/285b70dddb1c76108e8ab6c4c5ceebae2023d568))
* **select:** unify orphan filename validation with delete path ([9adea49](https://github.com/TheOrdinaryWow/oc-clean/commit/9adea495d98b920e3800a676579cb5546caec2d0))
* **vacuum:** handle interrupts and unavailable mode ([413ae5c](https://github.com/TheOrdinaryWow/oc-clean/commit/413ae5c06240da82415580d87767156da68c17d4))


### Performance

* **clean:** run read-only phases concurrently ([75ab313](https://github.com/TheOrdinaryWow/oc-clean/commit/75ab313dfd09624689919a30f7693e3dbda09545))
* **delete:** join dangling candidates in sql ([a3cbb9a](https://github.com/TheOrdinaryWow/oc-clean/commit/a3cbb9acb135c669de569624b0e72b807c2982e6))
* **doctor:** run independent checks concurrently ([38d703e](https://github.com/TheOrdinaryWow/oc-clean/commit/38d703e63b9cda27703e6d6936b00226df30efe0))
* **report:** aggregate impact rows in SQL ([d137dae](https://github.com/TheOrdinaryWow/oc-clean/commit/d137daeecedeb457fcdfbabe5cb5b10d62dce08e))


### Refactoring

* **parallel:** unify command implementations ([efafaa0](https://github.com/TheOrdinaryWow/oc-clean/commit/efafaa01987ef992d53ee9c569d6ca6be1e800ee))
* **reclaim:** isolate Windows rename FFI ([da54ca5](https://github.com/TheOrdinaryWow/oc-clean/commit/da54ca5602f33f2c1dc53c0ce1ae66ea5abd973a))


### Documentation

* add AGENTS.md and README ([df40c6b](https://github.com/TheOrdinaryWow/oc-clean/commit/df40c6b8f05f596c55af5b46d9de29e09c3b957c))
* add dual MIT and Apache-2.0 licensing ([d6db751](https://github.com/TheOrdinaryWow/oc-clean/commit/d6db7513c01d12c0c864368d14ab99e38b3627ca))
* add simplified chinese readme and cross-link both versions ([3bb410f](https://github.com/TheOrdinaryWow/oc-clean/commit/3bb410fa53b2a9e289bd61a6f77887d37a25cff3))
* document the log switch, session descriptions, and failure objects ([7435985](https://github.com/TheOrdinaryWow/oc-clean/commit/7435985ef8bb33af24c89f0e3c9d6a70abf78324))
* **readme:** document full OCC_* env coverage ([75fe04a](https://github.com/TheOrdinaryWow/oc-clean/commit/75fe04a4fa517b45407d0dc36ab1ad92d9ce399c))
* refresh agent guide, README development section, and add contributing guide ([4e60e0d](https://github.com/TheOrdinaryWow/oc-clean/commit/4e60e0da14aceb7bec9e740585f33d7bdc9e8be6))
* rewrite README with usage, safety model, and roadmap ([ea73a5a](https://github.com/TheOrdinaryWow/oc-clean/commit/ea73a5aee1eaa373bbed076fb47cba4d673943ff))
* state the supported OpenCode version and how a mismatch is reported ([7753fa0](https://github.com/TheOrdinaryWow/oc-clean/commit/7753fa0139619840b562067199f3af474275ae9c))
