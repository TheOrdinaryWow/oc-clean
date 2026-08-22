# oc-clean

[English](README.md) | 简体中文

`oc-clean` 是一个 Rust 命令行工具，用于检查、清理和压缩 OpenCode 的 SQLite 数据库及其关联存储。它提供只读分析与诊断、试运行清理规划、有界的会话删除、孤儿数据清理、快照垃圾回收，以及数据库空间回收。

## 为什么需要这个工具

OpenCode 把会话、消息、片段、事件及相关状态存放在 SQLite 中。会话没有自动过期机制，OpenCode 不会执行 `VACUUM`，而且会话消失时 `event` 行会被遗留下来，因为这些行不会随会话删除级联清除。因此在长期正常使用中，主数据库会无限增长，达到几十 GB。

仅仅删除行只会把页面放进 SQLite 的空闲列表，通常并不会缩小数据库文件。`oc-clean` 会删除选中的关系数据，并可以用 `VACUUM INTO` 重建数据库，或者在数据库已配置为 `auto_vacuum=INCREMENTAL` 时使用增量 vacuum。

> `clean` 和 `vacuum` 会修改数据库。两者都会先打印影响范围，等待交互式输入 `yes` 之后才动手；`--dry-run` 打印同样的报告后直接退出。在独立核对结果之前请保留生成的备份，并在执行破坏性操作前停止 OpenCode。

## OpenCode 版本兼容性

`oc-clean` 与一份 OpenCode schema 强绑定，当前面向的是 **OpenCode 1.18.19**。该 schema 从发行二进制中提取并提交为 `src/db/opencode_schema.sql`，文件头记录了来源版本、sha256 以及提取命令。工具发出的每一条 SQL 都直接引用这些表和列，所以这个绑定是实质性的，不只是名义上的。

绑定并不要求版本号完全相等。只要 OpenCode 的新版本没有改动 `oc-clean` 读取的那些表和带类型的列，就能正常工作；OpenCode 自身的 schema 在全新安装和升级过的数据库之间本来就有差异，两种形态都受支持。真正决定成败的是工具依赖的对象是否仍然存在、类型是否仍然一致。

`doctor` 不触碰数据就能回答这个问题，升级 OpenCode 之后应当首先运行它：

```sh
oc-clean doctor
```

它的 `Schema Compatibility` 一节会报告 [Schema 严格程度](#schema-严格程度) 描述的全部三个层级，并且刻意比其他命令宽松：只有第 1 层失败才会中止它，因此其他命令拒绝执行的数据库仍然可以被诊断。

| `doctor` 报告的内容 | `doctor` | `analyze`、`clean`、`vacuum` | 含义 |
|---|---|---|---|
| Tier 1 required — findings | 退出码 4 | 退出码 4 | SQL 所需的某张表或某个带类型的列缺失或类型不兼容。没有任何开关可以绕过，包括 `--force-schema`。 |
| Tier 2 extensions — findings | 退出码 0 | 退出码 0 | 出现了工具从不读取的未知表、列或索引。这是 OpenCode 新版本增加了东西时的正常形态。 |
| Tier 3 semantics — findings | 退出码 0 | 退出码 4 | 存在已验证契约未描述的外键、触发器或视图，删除语义可能与实测行为不同。`--force-schema` 可以把它降级为警告。 |

第 1 层失败会指明具体对象，例如 `missing required column session.time_archived`。在多版本支持落地之前，请把它当作「这个构建不支持那个 OpenCode 版本」，而不是需要设法绕过的问题：改用面向那个 OpenCode 版本构建的 `oc-clean`，或等待相应版本发布。任何情况下 `analyze` 和 `doctor` 都保持只读，因此诊断一个未知数据库始终是安全的。

## 从源码安装

本项目使用 Rust edition 2024，声明最低支持版本为 Rust 1.85，并基于 nightly 开发。`rust-toolchain.toml` 已固定该工具链，因此在检出目录内运行命令时 rustup 会自动选择并安装它。从检出的源码树安装二进制：

```sh
cargo install --locked --path .
oc-clean --help
```

优化后的二进制以 `oc-clean` 之名安装到 Cargo 的二进制目录，通常是 `~/.cargo/bin`。

## 数据库选择

数据库的实际优先级依次为：显式的 `--db PATH`、`OCC_DB`、OpenCode 已有的 `OPENCODE_DB`，最后是平台默认路径。`OCC_DB` 是 `--db` 对应的 clap 环境变量绑定；`OPENCODE_DB` 仍是 OpenCode 自己的数据库选择器，可以是绝对路径，也可以相对于 OpenCode 数据目录。特殊值 `:memory:` 会为 `analyze` 和 `doctor` 创建一个全新的内存态 OpenCode schema。破坏性命令要求使用基于文件的数据库。

在没有覆盖设置时，Linux 和 macOS 会把 latest 通道解析为 `~/.local/share/opencode/opencode.db`，并受 `XDG_DATA_HOME` 影响；Windows 使用 `USERPROFILE` 下对应的 OpenCode 数据目录。当不存在数据库路径覆盖时，`--channel NAME` 或 `OCC_CHANNEL` 会为自定义通道选择 `opencode-<NAME>.db`；`latest`、`beta` 和 `prod` 继续使用 `opencode.db`。派生出的外部路径是同级的 `storage`、`snapshot`、`tool-output` 和 `log` 目录。

```sh
oc-clean --db /srv/opencode/opencode.db analyze
OCC_DB=/srv/opencode/opencode.db oc-clean doctor
oc-clean analyze --db :memory: --json
OCC_CHANNEL=nightly oc-clean analyze
OPENCODE_DB=opencode-beta.db oc-clean analyze
```

## 快速开始

先运行完整报告，诊断安全状况，用 `--dry-run` 预览一次保守的清理，然后去掉它重复执行完全相同的命令来确认并执行：

```sh
oc-clean analyze
oc-clean doctor
oc-clean clean --older-than 90D --dry-run
oc-clean clean --older-than 90D
```

第二条命令会打印同样的影响范围，询问 `Proceed? [y/n]`，只有输入 `y` 或 `yes` 之后才执行删除，输入 `n` 或 `no` 则取消。既不是肯定也不是否定的回答会重新提问，每个问题一共三次机会。自动化场景必须在检查过同样的 `--dry-run` 选择结果之后，显式加上 `--dangerously-skip-confirm`。

## 命令

### `analyze`

`analyze` 以只读方式打开数据库，默认输出四个部分，顺序按照"决定删什么"这件事需要的先后排列：

1. **Database File Space** — 总量、活跃数据、空闲列表、WAL 与 SHM 字节数。
2. **Largest Sessions** — 体积最大的会话及其标题和所属项目。
3. **Project Attribution** — 按项目汇总的字节数。
4. **Age Distribution** — 按年龄段划分的会话数与字节数。

`--detailed` 会追加四个技术性部分，它们描述的是"数据库作为数据库"的状态，而不是"哪些会话可以删"：

5. **Orphan Census** — 此前删除操作遗留的行与文件。
6. **External Directories** — storage、snapshot、tool-output、log 四个目录下的文件数与字节数。
7. **Table and Index Space** — 按对象统计的字节占用。
8. **Row Counts** — 每张应用表的行数。

不带这个开关时，这四项是被跳过而不是被隐藏：对象空间统计要遍历 `dbstat`，孤儿普查要 stat 外部目录下的每个文件。在已提交的 1.9 GB 基准夹具上，标准报告耗时 454 ms，完整报告 1,165 ms。

报告中的每个会话除体积外还会给出标题、所属项目路径、最后活跃时间和消息数量，因为会话 ID 只是一串随机字符，无法说明会话内容。项目汇总同样以绝对 worktree 路径为主键，项目 ID 保留在旁边一列。路径超出终端宽度时会从开头截断，让用于区分不同检出的末尾目录保持可见。这些描述信息只会为报告实际展示的会话查询，因此 `--top` 决定了它们的开销上限。

```sh
oc-clean analyze
oc-clean analyze --top 25
oc-clean analyze --detailed
oc-clean analyze --detailed --json
oc-clean analyze --json --log json
```

### `doctor`

`doctor` 以只读方式打开数据库，报告 schema 兼容性、SQLite 完整性、外键完整性、孤儿数据普查、当前持有者扫描、重建所需余量、auto-vacuum 模式，以及时间戳单位的合理性。完整性检查或时间戳检查失败会返回退出码 7。

```sh
oc-clean doctor
oc-clean doctor --json
OCC_DB=/var/lib/opencode/opencode.db oc-clean doctor
```

### `clean`

`clean` 至少需要一个选择器：`--older-than`、`--include`、`--exclude`、`--larger-than`、`--archived` 或 `--orphans`。多个会话谓词之间取交集，子树选择会保持父子一致性。`--include` 和 `--exclude` 是同一个项目路径谓词的两个方向，不能同时使用，同时给出会被判为用法错误。`--keep-recent N` 保护每个项目中最近活跃的 N 个根会话，按项目分别计数，数的是根会话而不是全部会话；默认值为 0，即不保留任何会话，选择结果就是选择器所描述的内容。

```sh
# 预览至少 90 天无活动的根会话子树。
oc-clean clean --older-than 90D --dry-run

# 预览大于 250 十进制 MB 的已归档子树。
oc-clean clean --archived --larger-than 250MB --dry-run

# 预览 glob 选中的项目路径下所有匹配的会话。
oc-clean clean --include '/work/legacy-*' --keep-recent 20 --dry-run

# 预览某个项目之外的全部会话，也就是同一个 glob 的补集。
oc-clean clean --exclude '/work/keep-this' --older-than 30D --dry-run

# 预览会话形态的孤儿事件、悬空会话和外部孤儿文件。
oc-clean clean --orphans --dry-run

# 删除已检查过的选择结果，并交互式确认。
oc-clean clean --older-than 6M --gc-snapshots

# 在检查过等价的试运行之后，从非交互任务中执行。
oc-clean clean --older-than 1Y --dangerously-skip-confirm --json
```

影响报告会在任何删除之前列出体积最大的待删会话及其标题与所属项目路径，条数由 `--top` 限制，让选择结果可以被辨认而不只是被计数。

确认之后的清理会以每批 2,500 个候选的有界事务删除会话，显式删除匹配的事件聚合，清理受影响的空项目，移除对应的存储和快照产物，运行完整性检查，并默认重建数据库。`--no-vacuum` 会提交删除但把空闲页留在数据库文件中。`--incremental` 使用增量 auto-vacuum，要求源数据库此前已按 `auto_vacuum=INCREMENTAL` 配置并重建过。`--gc-snapshots` 还会压缩保留下来的快照仓库。

### `vacuum`

`vacuum` 回收 SQLite 已有的空闲列表空间，不选择也不删除应用数据行。默认模式使用经过校验的 `VACUUM INTO` 重建加原子替换；`--incremental` 则在已经使用增量 auto-vacuum 的数据库上请求有界的增量 vacuum 批次。与 `clean` 一样，它会打印报告并要求交互式输入 `yes`，`--dry-run` 则在报告之后停止。

```sh
# 预览重建策略、预计压缩后的体积和所需余量。
oc-clean vacuum --dry-run

# 重建、交互式确认，并保留带时间戳的备份。
oc-clean vacuum

# 在数据库支持的情况下预览并执行增量回收。
oc-clean vacuum --incremental --dry-run
oc-clean vacuum --incremental
```

## 选项

下表是当前 clap 接口定义的全部长选项。`Global` 选项在每个子命令之前或之后都可以接受；每个命令专属的行只作用于所属的子命令。

| 作用域 | 选项 | 环境变量 | 用途 |
|---|---|---|---|
| Global | `--version` | 仅命令行 | 打印 `Cargo.toml` 中记录的版本号后退出，短选项为 `-V`。 |
| Global | `--db <PATH>` | `OCC_DB` | 选择数据库路径，或用 `:memory:` 指定一个全新的内存分析目标。优先级高于 `OPENCODE_DB` 和平台发现。 |
| Global | `--channel <NAME>` | `OCC_CHANNEL` | 选择用于推导默认数据库文件名的 OpenCode 通道。 |
| Global | `--log <MODE>` | `OCC_LOG` | 选择 stderr 诊断输出：`off`、`text` 或 `json`，默认 `off`。设置 `RUST_LOG` 会隐式选择 `text`。 |
| Global | `--dry-run` | `OCC_DRY_RUN` | 打印 `clean` 或 `vacuum` 的报告后退出，不做任何修改，也不提示确认。 |
| Global | `--force` | 仅命令行 | 对已应用的 `clean` 或 `vacuum`，把「存在持有者」或「无法判定」的持有者关卡降级为警告。 |
| Global | `--force-schema` | 仅命令行 | 把第 3 层 schema 语义发现降级为警告；第 1 层仍然强制，第 2 层本就被容忍。 |
| Global | `--dangerously-skip-confirm` | 仅命令行 | 跳过已应用的 `clean` 和 `vacuum` 所需的交互式确认，包括 JSON 输出和管道执行场景。 |
| Global | `--skip-backup` | 仅命令行 | 在完整重建成功后删除临时回滚副本，而不是保留默认的 `.bak` 文件。 |
| analyze | `--json` | `OCC_JSON` | 在 stdout 输出一个稳定的 JSON 报告。 |
| analyze | `--top <N>` | `OCC_TOP` | 限制最大会话汇总的条数，默认 `10`。 |
| analyze | `--detailed` | `OCC_DETAILED` | 追加孤儿普查、外部目录、对象空间与行数统计。 |
| doctor | `--json` | `OCC_JSON` | 在 stdout 输出一个稳定的 JSON 诊断报告。 |
| clean | `--older-than <AGE>` | `OCC_OLDER_THAN` | 选择最近活动时间至少达到该粗粒度年龄的会话子树。 |
| clean | `--include <PATH_OR_GLOB>` | `OCC_INCLUDE` | 选择项目路径或 glob 匹配的会话。与 `--exclude` 互斥。 |
| clean | `--exclude <PATH_OR_GLOB>` | `OCC_EXCLUDE` | 选择项目路径或 glob 不匹配的会话。与 `--include` 互斥。 |
| clean | `--larger-than <SIZE>` | `OCC_LARGER_THAN` | 选择可归属负载达到该十进制体积的会话子树。 |
| clean | `--archived` | `OCC_ARCHIVED` | 选择已归档的会话。 |
| clean | `--orphans` | `OCC_ORPHANS` | 纳入会话形态的孤儿事件、悬空会话和孤儿外部存储。 |
| clean | `--keep-recent <N>` | `OCC_KEEP_RECENT` | 每个项目保留这么多最近活跃的根会话，默认 `0`，即不保留任何会话。 |
| clean | `--incremental` | `OCC_INCREMENTAL` | 在已使用 `auto_vacuum=INCREMENTAL` 的数据库上用增量 vacuum 回收空闲页。 |
| clean | `--no-vacuum` | `OCC_NO_VACUUM` | 提交选中的删除，跳过页面回收。 |
| clean | `--gc-snapshots` | `OCC_GC_SNAPSHOTS` | 清理之后压缩保留下来的快照仓库。 |
| clean | `--prune-empty-projects` | `OCC_PRUNE_EMPTY_PROJECTS` | 同时清理在本次清理之前就已经为空的项目。 |
| clean | `--top <N>` | `OCC_TOP` | 限制删除前列出的待删会话预览条数，默认 `10`。 |
| clean | `--json` | `OCC_JSON` | 在 stdout 以 JSON 输出清理报告。 |
| vacuum | `--json` | `OCC_JSON` | 在 stdout 以 JSON 输出 vacuum 报告。 |
| vacuum | `--incremental` | `OCC_INCREMENTAL` | 用增量 vacuum 回收空闲列表页面，而不是完整重建。 |

## 环境变量

可执行文件名为 `oc-clean`，而它自己的环境变量前缀是 `OCC_`。当前 clap 接口为全部四个子命令的每个非破坏性选项都提供了 `OCC_*` 环境变量绑定；破坏性开关有意要求在命令行上可见地传入。

开关类变量把 `1`、`true`、`yes`、`y`、`t`、`on` 读作启用，把 `0`、`false`、`no`、`n`、`f`、`off` 读作关闭，大小写不敏感。不在这两组之内的取值会以退出码 2 拒绝，而不是猜测其含义。`--help` 里只列出 `true` 和 `false`，因为把十二种写法全部列出只会让输出变长而不增加信息。

`OCC_CHANNEL` 是 `--channel` 的环境变量等价物；显式的 `--db`/`OCC_DB` 和 `OPENCODE_DB` 路径选择器优先于通道命名。`OPENCODE_DB` 属于 OpenCode，在 `--db` 和 `OCC_DB` 之后参与数据库发现回退。`XDG_DATA_HOME`、`HOME`、`USERPROFILE` 和 `OPENCODE_DISABLE_CHANNEL_DB` 也可能影响平台发现。`NO_COLOR` 会关闭人类可读报告输出中的颜色。`RUST_LOG` 用于选择 tracing 过滤级别，并在未设置 `--log`/`OCC_LOG` 时隐式启用 `text` 诊断输出。

## 时长与体积语法

`--older-than` 接受 `<整数><单位>`，单位为 `D`、`W`、`M` 或 `Y`。`D` 和 `W` 不区分大小写；`M` 必须大写，因为 `M` 表示固定的 30 天月份，而小写 `m` 会让人误解为分钟；`Y` 表示固定的 365 天年份。示例有 `30D`、`12w`、`6M` 和 `1Y`。取值使用受检整数运算，拒绝负数、小数、缺少单位、非 ASCII 数字、不支持的单位、溢出，以及小时、分钟、秒等一切小于一天的单位。

`--larger-than` 接受 `<数字><单位>`，单位为不区分大小写的 `MB` 或 `GB`。单位是十进制 SI：`1MB = 1,000,000 字节`，`1GB = 1,000,000,000 字节`。当小数能换算为整数字节时可以接受，例如 `1.5GB`；取值拒绝负数、缺少单位、非 ASCII 数字、格式错误的小数、溢出，以及 `MiB` 和 `GiB` 这类二进制单位。

小于一天的时长单位和二进制体积单位都被明确拒绝。增量 vacuum 内部的页面批次与这些面向用户的语法无关。

`--include` 和 `--exclude` 接受字面路径或 glob。当取值包含 `*`、`?` 或 `[` 时进入 glob 模式：`*` 匹配任意字符序列（含路径分隔符），`?` 匹配单个字符，`[abc]` 和 `[a-z]` 匹配字符类，方括号内以 `!` 开头表示取反。匹配前会去掉结尾的路径分隔符，因此 `/work/repo/` 和 `/work/repo` 是同一个模式。大小写策略跟随平台文件系统，macOS 和 Windows 上不区分大小写，Linux 上区分。模式会同时与每个项目的 worktree 和该项目登记的每个目录比较，只要命中其中之一，该项目的全部会话都会被选中，与每个会话自身的工作目录无关。

## 安全模型

### 试运行与确认

`analyze` 和 `doctor` 是只读的。`clean` 和 `vacuum` 会修改数据，两者都会停在交互式确认上，展示完整影响范围。`y` 和 `yes` 表示继续，`n` 和 `no` 表示取消，大小写和首尾空白都会被忽略。其他回答会重新提问，每个问题允许三次尝试，用尽后放弃执行。取消会以普通提示信息呈现，而不是 `error:` 行，因为拒绝是一个决定；退出码仍然是 2，让脚本能够判断什么都没执行。`--dry-run` 会完成同样的选择、兼容性、持有者和余量检查，打印报告后退出，同时保持数据库字节不变。会修改数据的运行在动手前获取 SQLite 的排他锁。管道输入、JSON 输出或缺少终端都无法回答确认提示，因此会以退出码 2 拒绝执行，除非提供 `--dangerously-skip-confirm`。

当一次 `clean` 选中的会话达到数据库全部会话的一半或更多时，第一个提示之后还会问第二个独立的问题，并列出选中数量与数据库总数的对比，两个回答都必须是肯定的才会执行。第二个问题有自己独立的尝试次数。`--dangerously-skip-confirm` 会同时绕过这两个提示，持有者、schema、锁、余量和完整性关卡全部保持有效。因此一个写错的选择器或数据库路径依然可能在无人值守时执行并删除错误的数据。

### 持有者检测

在清理或回收之前，平台检查器会扫描数据库、WAL 和 SHM 路径，并报告 `CompleteForVisibleProcesses`、`PartialDueToPermissions` 或 `Unsupported` 之一。Linux 使用可见的 `/proc` 文件描述符，macOS 使用 `libproc`，Windows 使用 Restart Manager。该扫描是一次时间点观测，只能看到当前账户和平台 API 可见的进程，并且无法阻止扫描之后有其他进程再连接上来。`CompleteForVisibleProcesses` 只覆盖可见进程，并不证明数据库处于静默状态。

会修改数据的 `clean` 和 `vacuum` 会拒绝「观测到持有者」，而「扫描结果无法判定」只在平台完全无法扫描（`Unsupported`）时才拒绝。报告为 `PartialDueToPermissions` 的扫描仍然覆盖了当前账户可见的每个进程，因此只发出警告并继续：非特权账户永远读不到其他用户的描述符表，仅凭这一点拒绝会挡住所有非 root 调用，却证明不了任何事情。`--force` 绕过剩余的前置拒绝，把它们变成警告；SQLite 锁获取、`data_version` 检查、schema 策略、磁盘余量、确认和完整性检查依然有效。对着活跃进程强制执行可能干扰 OpenCode、与外部文件清理产生竞态，或让写入仍附着在被替换数据库 inode 的句柄上。

### Schema 严格程度

第 1 层包含 SQL 实现所必需的表和带类型的列。缺失或类型不兼容的第 1 层对象总会中止命令，且无法绕过。第 2 层包含未知索引之类可容忍的扩展，只产生警告。第 3 层包含可能改变删除语义的外键、触发器和视图，默认会中止那些强制检查兼容性的命令。

`--force-schema` 只把第 3 层的发现降级为警告。它绝不会绕过第 1 层，也不改变第 2 层的处理方式。在不熟悉的触发器、视图或外键上继续执行，可能删除额外的行、保留本应删除的行，或以已验证 schema 契约之外的语义执行。

### 磁盘、完整性与中断

完整重建会在破坏性操作之前做空闲空间余量检查；空间不足时在交互式和非交互式模式下都返回退出码 6。该检查包含预计的存活数据、适用时的一批删除事务 WAL 配额、备份副本回退，以及 10% 的余量。`--force`、`--force-schema` 和 `--dangerously-skip-confirm` 都不能绕过这道关卡。增量 vacuum 不存在第二份完整数据库，在结构上遵循它自己的适用性检查。

清理使用有界事务，在关系数据删除后运行 SQLite 完整性和外键检查，在替换前校验重建产物，并在替换后校验已安装的数据库。修改前的中断不会留下任何变更；删除过程中的中断会在当前已提交批次之后停止，并以退出码 8 报告已完成的工作。重建替换失败时会尝试回滚，退出码 11 表示替换校验和回滚都失败，需要从报告中指明的路径手动恢复。

## 备份与磁盘峰值占用

默认完整重建成功后，会把原数据库保留为同级文件，命名为 `opencode.db.bak.YYYYMMDDTHHMMSSZ`。`.bak` 文件永远不会被自动删除；备份的保留和删除由操作者负责。增量 vacuum 和 `clean --no-vacuum` 不会创建这种重建备份。

设 `O` 为原数据库大小，`L` 为删除后预计的存活字节，`W` 为一批删除事务的 WAL 配额，`margin = ceil(0.10 * L)`。在支持硬链接时，额外所需空闲空间是 `L + W + margin`，因此文件系统峰值占用约为 `O + L + W + margin`；保留的 `.bak` 与原路径最初共享同一个 inode。当硬链接不可用时，备份会回退为完整复制，额外所需空闲空间是 `L + O + W + margin`，峰值占用约为 `2O + L + W + margin`。独立运行的 `vacuum` 取 `W = 0`，并用它当前的存活字节估算值作为 `L`。

`--skip-backup` 在余量计算中把备份副本字节视为零，并在校验成功后删除临时回滚副本。此后若出现问题，本次运行不会留下原数据库，恢复将依赖于独立的备份。

## 输出与自动化

人类可读报告输出到 stdout，其余内容都输出到 stderr。`--json` 会在 stdout 输出一个稳定的报告对象。

诊断输出默认关闭，因此常规运行的 stderr 保持干净，上面只会绘制进度条。`--log text` 或 `--log json` 会打开 tracing 诊断；设置 `RUST_LOG` 则隐式选择 `text`。当使用 `--json` 或 stderr 不是终端时，进度渲染会被抑制，因此重定向运行不会捕获到任何重绘序列。

失败输出不依赖 `--log`。命令失败时会向 stderr 写一行 `error:`，在存在明确后续动作时再写一行 `hint:`；使用 `--json` 时改为向 stderr 写一个可解析的失败对象，包含 `kind`、`exit_code`、`message` 和可选的 `hint`。字段契约见 [docs/json-report.md](docs/json-report.md)。

```sh
oc-clean analyze --json > analysis.json
oc-clean clean --older-than 120D --dry-run --json > preview.json
oc-clean clean --older-than 120D --dangerously-skip-confirm --json > result.json
```

JSON 报告契约记录在 [docs/json-report.md](docs/json-report.md)（英文）。

## 实测性能

已提交的基准使用一个目标为 2,000,000,000 字节的生成夹具，实际达到 1,962,291,200 字节，包含 2,675 个会话、53,500 条消息和 214,000 个片段。记录到的 `--detailed` 分析冷启动耗时 1,165.335 ms，热态 1,123.208 ms；标准分析耗时 453.755 ms。这些是来自 `benchmarks.json` 的实测回归参考值，本地结果会受主机硬件、文件系统、SQLite 行为、保留数据形态和缓存状态影响。

在同一个 2,675 会话夹具上进行的删除批次调优结果：

| 候选批次大小 | 耗时 | 事务数 |
|---|---:|---:|
| 1,000 | 3,198.648 ms | 3 |
| 2,500 | 2,934.798 ms | 2 |
| 5,000 | 3,035.092 ms | 1 |
| 10,000 | 3,187.325 ms | 1 |

在本次测量中 2,500 候选批次最快，也是当前的删除默认值。在本地复现大夹具回归测试：

```sh
cargo test --release --features bench-large --test perf performance_budgets_hold_on_the_committed_large_fixture_scale -- --ignored --nocapture --test-threads=1
```

## 退出码

退出码是稳定的进程契约。有几个错误变体有意共享同一个类别码。

| 退出码 | 错误变体 | 含义 |
|---:|---|---|
| 0 | `Success` | 完全成功。 |
| 1 | `Io` | 文件系统、终端或输出 I/O 失败。 |
| 1 | `Sqlite` | 不属于更具体类别的通用 SQLite 失败。 |
| 2 | `InvalidArgument` | 取值非法、缺少选择器，或调用方式不兼容。 |
| 2 | `Canceled` | 操作者拒绝了确认，或始终没有作答。 |
| 3 | `NotFound` | 数据库文件不存在。 |
| 4 | `SchemaIncompatible` | 必需的 schema 缺失，或删除语义无法识别。 |
| 5 | `DatabaseBusy` | 持有者策略、SQLite 加锁或并发变更保护拒绝了该操作。 |
| 6 | `InsufficientDiskSpace` | 完整重建的余量低于计算出的需求。 |
| 6 | `ReclaimUnavailable` | 请求的回收策略不可用，包括增量 vacuum 前置条件不满足。 |
| 7 | `IntegrityCheckFailed` | SQLite 完整性、外键完整性或时间戳合理性检查失败。 |
| 8 | `Interrupted` | SIGINT 中止了操作；消息中会说明已完成的工作。 |
| 9 | `UnsupportedPlatform` | 该平台没有受支持的实现。 |
| 10 | `PartialSuccess` | 主要工作已完成，但部分外部清理被遗留。 |
| 11 | `SwapRollbackFailed` | 数据库替换校验失败且回滚也失败，需要手动恢复。 |

## 本工具不做的事

- 自动调度、后台保留策略和守护进程运行都不在这个命令行工具的范围内；由操作者决定何时运行。
- 数据库静默仍然是操作者的责任；持有者检测只提供一个时间点的、可见性受限的安全信号。
- 增量 vacuum 仅是面向已配置该模式的数据库的有界空闲列表回收策略；完整重建提供另一套压缩与校验行为。
- Schema 迁移不在范围内；第 1 层不兼容的数据库需要一个兼容的版本，或者一次单独评审过的迁移。
- 让单个二进制支持多个 OpenCode schema 版本仍属于路线图工作；某个构建只面向 [OpenCode 版本兼容性](#opencode-版本兼容性) 中指明的那一个版本。
- 备份生命周期管理不在范围内；默认的 `.bak` 文件会一直保留到操作者删除为止。
- 关闭正在运行的 OpenCode 进程不在范围内；请在应用清理或回收之前自行停止 OpenCode。
- AFT 与 Magic Context 的清理集成仍属于路线图工作。

## 路线图

- 期望但尚未构建多版本 OpenCode 支持。目前一个构建只面向一份 schema，因此 OpenCode 若改动了 `oc-clean` 读取的表或列，就需要一个与之匹配的 `oc-clean` 版本。让单个二进制同时支持多个 schema 修订可以解除这层耦合。
- 期望但尚未构建 AFT 清理集成。
- 期望但尚未构建 Magic Context 清理集成。

## 开发

默认质量关卡使用固定的 nightly 工具链、rustfmt、Clippy 已配置的 `all` 与 `pedantic` lint 组、cargo-nextest，以及至少 80% 的行覆盖率。`Taskfile.yml` 对它们做了封装；不带参数运行 `task` 可以列出全部目标。

```sh
task ci         # 依次执行 fmt:check、lint、test
task test       # cargo nextest run --no-tests=pass
task test:doc   # 文档测试，nextest 不会运行它们
task coverage   # 针对 80% 下限的行覆盖率报告
task lint       # clippy --all-targets --all-features -- -D warnings
task audit      # cargo audit 依赖公告扫描
```

等价的 cargo 直接调用是：

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --no-tests=pass
cargo test --doc
cargo llvm-cov --all-features nextest --no-tests=pass --fail-under-lines 80
```

测试会在临时目录下创建一次性的 SQLite 夹具。开发和测试绝不能指向真实的 OpenCode 数据库。

持续集成会在 Ubuntu、macOS 和 Windows 上原生执行 fmt、lint、build、test 和文档测试序列，因为持有者检测在每个平台上都有各自的实现。另有独立作业负责强制覆盖率下限和扫描依赖公告。

提交格式、分支与合并请求流程，以及发布自动化，都记录在 [CONTRIBUTING.md](CONTRIBUTING.md)（英文）中。

## 许可证

本项目以 [Apache License, Version 2.0](LICENSE-APACHE) 或 [MIT License](LICENSE-MIT) 双许可发布，你可以任选其一。

除非你明确另行声明，否则你有意提交并包含进 `oc-clean` 的任何贡献（按 Apache-2.0 许可证的定义）都将以上述双许可发布，不附加任何额外条件。
