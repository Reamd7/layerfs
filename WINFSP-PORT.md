# LayerFS × WinFsp 移植报告(实验分支)

日期:2026-10-24 · 基线:main @ `4497b0cbf6d`(v1.1.0) · 移植提交:`33e1eb188`
会话存档:`WINFSP-PORT.session/`(产生本移植的完整 agent 会话)

## 一句话状态

LayerFS 核心闭环(挂载 → 卷内读写 → commit → 历史持久化)已在**原生 Windows**
(无 WSL、无 Docker)真机跑通;daemon 控制面与 JS SDK 未接,一致性/性能测试未跑。

## 进度快照(对照 PR #427 macOS/NFS 同构工作)

| 里程碑 | PR #427(macOS/NFS) | 本分支(Windows/WinFsp) |
|---|---|---|
| 内核桥 crate | layerfs-nfs 2,669 行(含 811 行合成内核补 NFS 无状态) | winfsp 模块 1,050 行(WinFsp 回调自带生命周期,无需合成内核) |
| Store 平台放行 | 2 文件(macOS 是 unix,import 层天然可编译) | 12 文件(Windows 非 unix:project import 全套移植 + persistence/overlay 门控) |
| 真实挂载 e2e | ✓ mount_nfs,语义矩阵 18/18 | ✓ WinFsp,两轮 e2e(commit 持久化验证),无矩阵 |
| daemon 控制面 | ✓ export.rs 972 行(`LAYERFS_KERNEL=nfs`) | ✗ 未做(有现成模板可平移) |
| SDK 可用 | ✓ native 路线 49/49 smoke | ✗ 只有 runner(example 级) |
| 韧性/并发测试 | ✓(杀 daemon、冲突、并发挂载) | ✗ |
| 上游合规 | ✓ 每 commit LOC 记账 | ✗ 本报告一份 |

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

## 剩余路线(按优先级,2026-10-24 修订)

1. **daemon 控制面接入(通往 SDK 的唯一缺口)**:平移 PR #427 的 `core/crates/layerfs-daemon/src/control/export.rs`(972 行)做 Windows 版——Attach/Unmount/Force 状态机、`ReadyMount` 回执(`abi_major=0`,`mount_id` 填挂载路径)、commit 前的 flush 等价物、`Kernel::Attaching` 去 Linux 限定。完成后 `LAYERFS_KERNEL=winfsp` 与 fuse/nfs 并列。
2. **Windows 版 names.rs**:拒 NTFS ADS(冒号流名)与保留名(CON/NUL/PRN…),别让协议残留进 Commit(对照 PR427 拦 `.nfs*`/`._*`)。
3. **readdir N+1 修复**:枚举循环每条目一次 LOOKUP+GETATTR,应批量。
4. **测试方法论抄 PR427**:同操作组"挂载 vs 本地盘"语义对比矩阵(18 项)、SDK smoke(49 项)、杀 daemon 韧性、并发挂载/冲突。
5. **SDK**:`api/binding`(napi)按需 Windows 化,或沿用 dsh 的 WSL worker。
6. **一致性**:winfsp-tests / pjdfstest 子集。
7. **上游合规**(若回馈):按仓库规矩逐 commit 生产 LOC 记账、边界脚本、merge-only 分支策略;`backing.rs` links bug 可单独先提。

## 许可警示

winfsp-rs(SnowflakePowered)为 **GPLv3**(非 MIT,未见 FLOSS 例外)。LayerFS 是 MIT,与之链接的二进制**分发时**需遵守 GPL。本地实验无碍;回馈上游前需换自维护的 winfsp.dll FFI 或取得例外授权(上游 owner 对同类平台放行——PR427 的 kernel_guard/F-020 争议——的裁决也会影响本分支能否上游化)。

## 关联工作

- **PR #427**(@jiangkoumo,macOS loopback NFSv3 原生路线,Draft):同缝同构的先行者;其 `export.rs` 是本分支路线 ① 的模板;与 owner 的 F-020("Store 只在 Linux")冲突待裁决,裁决结果同样约束本分支。
- **openai/codex**(Apache-2.0):agent 沙箱分层参考(RestrictedToken/ACL/WFP/MXC + egress proxy)。
- 上游 LayerFS 沙箱现状:Docker 容器(CAP_SYS_ADMIN + /dev/fuse + 防命名空间逃逸 seccomp),无 egress 控制——是"环境隔离器"而非安全沙箱,与本分支裸跑决策互补。
