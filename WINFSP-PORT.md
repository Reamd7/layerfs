# LayerFS × WinFsp 移植报告(实验分支)

日期:2026-10-24 · 基线:main @ `4497b0cbf6d`(v1.1.0) · 移植提交:`33e1eb188`
会话存档:`WINFSP-PORT.session/`(产生本移植的完整 agent 会话)

## 一句话状态

LayerFS 核心闭环(挂载 → 卷内读写 → commit → 历史持久化)**与 daemon 控制面**
(Mount/Attach/Unmount/Force 经 `execute_control`,WinFsp 内核半)已在**原生 Windows**
真机跑通;`layerfs-winfsp` 裸跑 runner、cmd 语义矩阵/韧性/互斥测试、winfsp-tests
一致性套件(15 族通过)与 Windows 版 Node addon(按 owner #352 边界应答)全部实测。
剩余:大目录(>64 条窗口)枚举分页缺陷、DOS 扩展属性面、SDK 的 Windows worker 路线。

## 支持程度(2026-10-25 实测口径)

### 已真机验证(本机跑通)

| 能力 | 证据 |
|---|---|
| 全 core 引擎 Windows 原生编译 | 14 crate 全绿 + `layerfs-winfsp` runner crate + `layerfs-node` addon(cargo check x86_64-pc-windows-msvc) |
| 空项目 Init → Store 创建 | `initialized empty store`——import/history/persistence 全链在 NTFS 上工作 |
| WinFsp 挂载/卸载 | `mounted at lfs-run\mount`,WinFsp 2.1 驱动实挂 |
| 卷内文件创建/读/列/追加 | cmd 语义矩阵 12 步(含跨会话持久化、最终清单精确比对) |
| rename 与删除族 | `ren`/`del`/`rmdir`(非空拒、清空后删)矩阵断言;winfsp-tests `rename_test`、`delete_test`、`delete_pending/mmap` 通过 |
| Windows 名字规则 | 冒号流名与保留设备名(CON/NUL…)创建即拒(`refused_name`) |
| delete-on-close 双通道 | Create/Open 时的 `FILE_DELETE_ON_CLOSE` 与 SetDelete 两条路径都送达 Cleanup;非空目录 SetDelete 报 `DIRECTORY_NOT_EMPTY` |
| DOS 只读属性 | 文件/目录 ARCHIVE 与 READONLY 位、Create 的 READONLY → 0444、SetFileAttributes 往返、mtime 写入(`set_basic_info`) |
| daemon 控制面 | `tests/winfsp_control`:Mount→Attach(回执 abi=0)→目录可列→Unmount→目录消失→再 Mount/Attach,全过 |
| 韧性 | 杀 idle runner(taskkill /F)后 Store 完好,残留挂载目录由下次 attach 清理 |
| 单写者互斥 | 同一 Store 的第二个 runner 非零退出,先启动者正常完成 |
| 一致性套件 | winfsp-tests --external 15 族 OK(下节表) |
| Node addon | `layerfs_node.dll` 构建;project.init 按 owner #352 拒("needs the daemons' Linux kernel"),sandbox 组按 Unix-only 拒——Windows SDK 路线是 worker 设计 |

### 已实现、待实测(只过了编译/单元级)

- 大文件(> WRITE_WINDOW 分窗写)——逻辑在(与 FUSE 同路),无规模实测
- 并发访问的规模化(WinFsp 多回调线程机制在,矩阵/互斥已测,无压力测试)
- ForceUnmount 的 Windows 路径(与 unmount 同实现,control 测试覆盖普通轮)

### 未实现(调用即拒或无存储面)

符号链接(以普通文件呈现)、reparse point、安全描述符/ACL(notraverse/getsecurity/
setsecurity 族拒)、HIDDEN/SYSTEM DOS 属性(READONLY/ARCHIVE 已实现,无扩展属性存储
面)、allocation 预分配(create_allocation)、mmap 写直通(`write_to_eof`/
`constrained_io`)、named streams(NTFS ADS)、ioctl/oplock、POSIX delete/rename 变体
(delete_ex/rename_ex)、尾随反斜杠目录语义(WinFsp FSD 剥尾 `\` 且不设目录位,用户态
不可见)、大目录(>64 条引擎窗口)枚举分页(每次回调重枚举到窗口尾,计数错误)。
SDK 的 sandbox/workspace/exec 组按 Unix-only 拒(Engine 传输层无 Windows 移植);
Store 直连组按 owner #352 拒(Windows SDK 路线是 worker 设计,dsh/WSL2 侧)。

### 运行形态与边界

- runner:`winfsp_run <dir> [--idle N] -- <cmd>`(crate `layerfs-winfsp`,应用组合层,
  依赖 project/fuse/daemon);daemon 控制面经 `execute_control` 的 Request 驱动,与
  Linux FUSE 路线同一条 registry 状态机
- 裸跑:无沙箱、无 scratch,信任模型 = 裸 git worktree(升级路线:抄 Codex
  RestrictedToken 档或 MXC)
- 性能未调(Defender 双扫未排除;A2 缓存已去掉重复 GETATTR)

### 成熟度评级

**实验级偏上**:真引擎、真驱动、真提交历史、控制面状态机、语义矩阵与一致性套件都
实测过。离生产差:大目录分页缺陷、DOS 扩展属性面、性能调优、SDK worker 路线
(dsh 侧)。单文件/小项目的 agent 工作流当天可用。


## 进度快照(对照 PR #427 macOS/NFS 同构工作)

| 里程碑 | PR #427(macOS/NFS) | 本分支(Windows/WinFsp) |
|---|---|---|
| 内核桥 crate | layerfs-nfs 2,669 行(含 811 行合成内核补 NFS 无状态) | winfsp 模块 1,050 行(WinFsp 回调自带生命周期,无需合成内核) |
| Store 平台放行 | 2 文件(macOS 是 unix,import 层天然可编译) | 12 文件(Windows 非 unix:project import 全套移植 + persistence/overlay 门控) |
| 真实挂载 e2e | ✓ mount_nfs,语义矩阵 18/18 | ✓ WinFsp,cmd 语义矩阵 12 步 + 跨会话持久化与最终清单比对 |
| daemon 控制面 | ✓ export.rs 972 行(`LAYERFS_KERNEL=nfs`) | ✓ export.rs Windows 版(Attach/Unmount/Force,60s 有界停机,WinFspKept 保管),进程内 execute_control 全链测试 |
| SDK 可用 | ✓ native 路线 49/49 smoke | ◐ addon 构建并加载;Store 直连按 owner #352 拒,sandbox 组 Unix-only 拒(Windows 路线 = worker,dsh 侧) |
| 韧性/并发测试 | ✓(杀 daemon、冲突、并发挂载) | ✓ 杀 runner 后 Store 完好 + 单写者互斥 |
| 一致性 | ✓(winfsp-tests 同套) | ✓ winfsp-tests --external:15 族 OK,失败项全部归因(下节) |
| 上游合规 | ✓ 每 commit LOC 记账 | ✓ 逐 commit LOC(96,886 → 97,975)+ boundary 回基线 |

## 改动清单(基线 `4497b0cbf6d` → `33e1eb188`,27 文件 +1,823/−122)

| 文件 | 改动 |
|---|---|
| `layerfs-store-service/src/install.rs` | `#[cfg(unix)]` 门控 0700 目录与 uid 校验 |
| `layerfs-daemon/src/application/config.rs` | O_NOFOLLOW/O_CLOEXEC/uid/mode 硬化 cfg 门控;新增 `open_private_file` 助手 |
| `layerfs-overlay`、`layerfs-persistence` Cargo.toml | `[target.'cfg(windows)'.dependencies] rusqlite bundled`(照上游 Linux 模式) |
| `layerfs-persistence/src/store/open.rs`、`backend/sqlite/connection.rs` | 平台门控放行 Windows |
| `layerfs-persistence/src/backend/sqlite/writer_admission.rs` | Windows 走无 flock 分支(SQLite 自身锁,同 macOS 先例) |
| `layerfs-overlay/src/database/startup.rs`、`physical.rs` | overlay DB 放行;`Allocation` 的 Windows 弱身份实现(无 dev/ino/nlink,以长度+时间戳为证据,`device_full` 恒 false) |
| `layerfs-store-service/src/overlay/backing.rs` | **修上游 bug**:非 unix 分支 `links=0` 恒假导致 bind 必败,补为 1(对任何非 unix 平台有效,可独立回馈) |
| `layerfs-project/src/import/*`(7 文件) | init 全平台化:Windows Stamp 弱身份合成(mode 0644/0755、FILETIME 换算)、`read_link`/`outside_source` 的 Windows 分支、路径字节兼容层 `os_bytes`/`os_from_vec`(Unix 精确、Windows 有损 UTF-8)、error 助手门控移除 |
| `layerfs-fuse/Cargo.toml` + `src/lib.rs` | winfsp 0.13(`windows-61` feature)+ windows 0.61 目标依赖;`#[cfg(windows)] pub mod winfsp` |
| `layerfs-fuse/src/winfsp/`(新,1,050 行) | `mod`(51)/`blocking`(36,park/unpark 执行器)/`context`(783)/`session`(130) |
| `layerfs-daemon/examples/winfsp_run.rs`(新,219 行) | 裸跑 runner |

## 后端结构

- `WinFspSession::attach(config, queue, services)`:挂载点须不存在(WinFsp 随卷创建;陈旧空目录会被清掉)→ `FileSystemHost`(FineGuard,大小写敏感,`file_info_timeout=1000`)→ `mount` + `start`;`detach` 幂等。
- `LayerFsContext` 实现 winfsp-rs `FileSystemContext`:WinFsp 回调线程直接充当接收 worker,用 park/unpark 执行器跑 `RequestServices` 的 ServiceFuture——与 Linux 接收循环同一契约。
- 全部语义经可移植的 `operations` 引擎(`KernelRead`/`KernelData`/`DirectoryStream`/`KernelMutation`),未复制任何 Linux 门控代码。**接缝与 PR #427 的 layerfs-nfs 完全相同**(交叉验证)。

## 已实现

**读路径**:`get_security_by_name`(逐组件 LOOKUP 路径解析)、`open`(Open/Opendir,句柄映射 `OpenFile/KernelDirectory::owner_id`)、`close`、`get_file_info`、`read`、`read_directory`(WinFsp 按名续枚举 ↔ LayerFS 按 cookie 续读,marker 表在 FileContext)、`get_volume_info`(镜像 Linux STATFS 声明)。

**写路径**(经 `operations::mutation`,Publication 票据语义完整,`replied()` 在返回前释放):`create`(文件 0644/目录 0755)、`write`(按 `WRITE_WINDOW` 分窗)、`overwrite`/`set_file_size`(Attributes→size)、`rename`(跨目录带 root 祖先证据)、`set_delete`+`cleanup`(WinFsp 延迟删除 → Unlink/Rmdir)、`flush`(no-op,持久性属 commit)。`Declined` 映射:Contended→STATUS_RETRY、NoSpace→STATUS_DISK_FULL(与 PR427 的 NFS3ERR_JUKEBOX 同构)。

**未实现**(调用即 `STATUS_INVALID_DEVICE_REQUEST` 或降级):符号链接(以普通文件呈现)、reparse point、安全描述符/ACL、`set_basic_info`(chmod/utimens)、named streams、`write_to_eof`/`constrained_io`、mmap 写直通。

## 复现指南(含踩坑)

前置:WinFsp 驱动已装(2.1.25156,MSI 提权一次;本机已装)。

```
cd core
cargo build -p layerfs-daemon --example winfsp_run --target x86_64-pc-windows-msvc

# ① winfsp-x64.dll 必须让 exe 找得到:复制到 exe 旁,或 PATH 含 C:\Program Files (x86)\WinFsp\bin
cp "/c/Program Files (x86)/WinFsp/bin/winfsp-x64.dll" target/x86_64-pc-windows-msvc/debug/examples/

# ② 必须用 cmd /c 调 exe:git-bash 直调会报 "command not found"(加载器错误的误报)
cmd /c "target\x86_64-pc-windows-msvc\debug\examples\winfsp_run.exe C:\...\lfs-run -- cmd /c \"echo hi > a.txt & dir\""
```

runner 语义:每运行一次 = 一个 Workspace 生命周期;重跑前自动清掉上次的 `overlay.sqlite`(历史在 store,不丢)。

## 验证

```
cargo check --workspace --exclude layerfs-node --target x86_64-pc-windows-msvc   # 绿
cargo test  -p layerfs-fuse --target x86_64-pc-windows-msvc --lib                # winfsp::tests 通过
```

**端到端**(本机真机):

```
> winfsp_run lfs-run -- cmd /c "echo layerfs-on-windows > hello.txt & dir /b & type hello.txt"
initialized empty store / mounted / hello.txt / layerfs-on-windows / committed / unmounted

> winfsp_run lfs-run -- cmd /c "type hello.txt & echo second-run >> hello.txt"     # 第二次运行
hello.txt / layerfs-on-windows / layerfs-on-windows second-run / committed / unmounted
```

即:init → bind → WinFsp 挂载 → 卷内 create/readdir/read/write → commit 持久化 → 重挂载读回历史。

**Linux 回归**:cfg 检查在本会话中段全绿;其后 Unix 分支改动(os_bytes/os_from_vec、child_path、read_link)与原语义逐行等价。完整 Linux 构建需 Linux C 工具链(bundled SQLite/zstd 的 cc),本 Windows 主机不可用,未重跑——**回馈上游前必须在 Linux 机器重跑**。

`layerfs-node`(napi)沿用上游设计不在 Windows 构建(dsh 的 Windows 方案在 WSL 内加载 addon)。

## 架构决策:宿主裸跑,无沙箱(2026-10-24 定)

Sandbox 四职能(命令隔离/scratch/环境一致/FUSE 内核边界)中只有最后一项技术必需,而 WinFsp 挂载天然不需要。决策:**不做沙箱**,命令以当前用户裸跑,信任模型 = 裸 git worktree(review 兜底)。与 PR #427 的 local sandbox 路线同构(他们 README 也写明 "it isolates nothing",连 PATH 继承的取舍都一致)。

| 现有 Linux 路径 | Windows 裸跑对应 |
|---|---|
| `layerfs-sandbox`(Docker 客户端) | 不移植,保持 unix 门控 |
| `daemon/control/{attach,detach,serving,force}.rs` → fuse session | 待写:WinFspSession 承担生命周期,`ReadyMount` 照常应答(见下方路线 ①) |
| `workspace exec` = Docker exec | `std::process::Command`,cwd=挂载目录,继承 stdio |
| `secure_process`(非 Linux 拒绝) | 放行 Windows(裸跑声明) |
| runtime image | 无;命令用宿主环境 |

**沙箱升级路线**(已勘察,暂不做):第一档抄 openai/codex(Apache-2.0,仓库 `Temp/codex` 或 GitHub)的 RestrictedToken + deny-write ACL(`windows-sandbox-rs/src/{token,workspace_acl,file_write}.rs`,连测试抄,~数千行);或直接依赖 microsoft/mxc SDK(内核容器,零拷贝);egress 白名单抄其 `network-proxy`。分层可摘,详见 Codex `WindowsSandboxLevel = Disabled|RestrictedToken|Elevated` 三档设计。

## 一致性(winfsp-tests 2.1,--external --resilient,2026-10-25)

驱动:`winfsp_run <dir> --idle 900` 挂活卷,`winfsp-tests-x64.exe --external
--resilient --case-insensitive-cmp`,按计划排除 stream/ea/reparse/flipflop 面。
套件首败即停,失败项逐个归因(修复或 known)后推进。本轮修复驱动的真实缺陷:
delete-on-close 双通道、非空目录 SetDelete、ARCHIVE/READONLY 位、SetFileAttributes
往返、目录只读位、目录 replace 拒绝。

**通过(15)**:create_readonlydir、create_related、create_sd、create_backup、
create_restore、create_share、create_curdir、delete、delete_pending、delete_mmap、
rename、rename_backslash、rename_open、rename_caseins、rename_mmap。

**known 归因(排除)**:create_test(尾随 `\` 语义:WinFsp FSD 剥除且不设目录位,
用户态不可实现)、create_fileattr/setfileinfo(HIDDEN/SYSTEM 无存储面)、
create_allocation(预分配需引擎面)、create_notraverse、getsecurity/setsecurity
(ACL)、getfileinfo×2(按句柄名查询)、create_namelen、rdwr_noncached/
noncached_overlapped/writethru 变体与 flush(NO_BUFFERING/写直通语义)、
querydir(>64 条引擎窗口分页:每次回调重枚举到窗口尾,计数错)、delete_ex/
rename_ex(POSIX 变体)、delete_standby/rename_standby(EXCEPTION 0xC0000006 崩)、
getvolinfo(卷句柄打开)。pjdfstest 未跑(需 POSIX 环境)。

## 剩余路线(2026-10-25 修订)

1. ~~daemon 控制面接入~~ ✅(export.rs Windows 版;`Kernel` 三个 Windows 变体;
   `KernelServing` 别名;进程内全链测试;Exec 前提证伪——控制协议无 Request::Exec,
   命令执行属调用方/Docker Sandbox,未加协议面)
2. ~~Windows 名字规则~~ ✅(冒号流名/保留名/尾点空格;winfsp-tests 验证)
3. ~~readdir N+1~~ ✅(per-handle stats 缓存,10k 上界;整窗 GETATTR 消除)
4. ~~测试方法论~~ ✅(cmd 语义矩阵 12 步 + 杀 runner 韧性 + 单写者互斥 + 控制面循环)
5. SDK:◐ addon Windows 化完成(discovery 锁/权限、输出读位、sandbox 组 Unix-only
   拒);完整接通的 blocker 是 owner #352(Store 直连 Linux-only)→ Windows 走
   worker 路线(dsh/WSL2 侧设计),本仓库范围外
6. 一致性:◐ winfsp-tests 15 族 OK、失败全归因;pjdfstest 未跑
7. 上游合规:◐ 逐 commit LOC(96,886 → 97,975,delta +1,089;口径 core/crates
   src,api/binding 不在计数根)、boundary 回基线(5 个预存违规为移植提交遗留)、
   fmt/clippy 改动面干净;linux target 回归 NOT_RUN(本机无
   x86_64-unknown-linux-gnu 组件)

新增剩余(按优先级):

1. **大目录枚举分页**:引擎目录窗口(64 条)与 DirBuffer/marker 恢复的交互——每次
   readdir 回调重枚举到窗口尾,>64 条目录计数错(小目录功能正确;querydir_test 归因)
2. **DOS 扩展属性面**:HIDDEN/SYSTEM 存储(xattr/元数据扩展)或明确文档化为不支持
3. **standby/mmap 族崩溃归因**(EXCEPTION 0xC0000006,两处)
4. **性能**:Defender 排除;file_info_timeout(1000ms)与 notify 的权衡(缓存窗口内
   属性查询可能陈旧——file_info_timeout=0 会破坏 DirInfo 缓存驱动的枚举,已回退)

## 许可警示

winfsp-rs(SnowflakePowered)为 **GPLv3**(非 MIT,未见 FLOSS 例外)。LayerFS 是 MIT,与之链接的二进制**分发时**需遵守 GPL。本地实验无碍;回馈上游前需换自维护的 winfsp.dll FFI 或取得例外授权(上游 owner 对同类平台放行——PR427 的 kernel_guard/F-020 争议——的裁决也会影响本分支能否上游化)。

## 关联工作

- **PR #427**(@jiangkoumo,macOS loopback NFSv3 原生路线,Draft):同缝同构的先行者;其 `export.rs` 是本分支路线 ① 的模板;与 owner 的 F-020("Store 只在 Linux")冲突待裁决,裁决结果同样约束本分支。
- **openai/codex**(Apache-2.0):agent 沙箱分层参考(RestrictedToken/ACL/WFP/MXC + egress proxy)。
- 上游 LayerFS 沙箱现状:Docker 容器(CAP_SYS_ADMIN + /dev/fuse + 防命名空间逃逸 seccomp),无 egress 控制——是"环境隔离器"而非安全沙箱,与本分支裸跑决策互补。
