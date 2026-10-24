# LayerFS × WinFsp 移植报告(实验分支)

日期:2026-10-24 · 基线:main @ `4497b0cbf6d`(v1.1.0 后)

## 目标

在不破坏 Linux 构建的前提下,让 LayerFS core 在 Windows 原生编译,并在
`layerfs-fuse` 内提供 WinFsp 挂载后端(替代 `/dev/fuse` + Docker Sandbox 路径)。

## 改动清单

| `core/crates/layerfs-store-service/src/install.rs` | `#[cfg(unix)]` 门控 0700 与 uid 校验 |
| `core/crates/layerfs-daemon/src/application/config.rs` | O_NOFOLLOW/uid/mode 硬化 cfg 门控 |
| `core/crates/layerfs-overlay` + `layerfs-persistence` Cargo.toml | `cfg(windows)` bundled rusqlite |
| `core/crates/layerfs-persistence/src/store/open.rs`、`backend/sqlite/connection.rs` | 平台门控放行 Windows |
| `core/crates/layerfs-persistence/src/backend/sqlite/writer_admission.rs` | Windows 走无 flock 分支(同 macOS) |
| `core/crates/layerfs-overlay/src/database/{startup,physical}.rs` | overlay DB 放行 Windows;Allocation 的 Windows 弱身份实现 |
| `core/crates/layerfs-store-service/src/overlay/backing.rs` | 非 unix links 补 1 |
| `core/crates/layerfs-project/src/import/*` | init 全平台化:Stamp/read_link/路径字节兼容层(os_bytes/os_from_vec,Windows 有损 UTF-8)、error 助手门控移除 |
| `core/crates/layerfs-fuse/Cargo.toml` + `src/lib.rs` | winfsp 0.13(windows-61)+ windows 0.61 目标依赖;`cfg(windows) pub mod winfsp` |
| `core/crates/layerfs-fuse/src/winfsp/`(新,~1200 行) | session/blocking/context:WinFsp 后端,读写两路 |
| `core/crates/layerfs-daemon/examples/winfsp_run.rs`(新) | 裸跑 runner:init→bind→mount→exec→commit→unmount |

## 新后端结构

- `WinFspSession::attach(config, queue, services)`:建挂载目录 → `FileSystemHost`(FineGuard,大小写敏感)→ `mount` + `start`;`detach` 幂等卸载。
- `LayerFsContext` 实现 winfsp-rs `FileSystemContext`:WinFsp 回调线程直接充当接收 worker,用 park/unpark 执行器(`blocking.rs`)跑 `RequestServices` 的 ServiceFuture——与 Linux 接收循环同一契约。
- 全部语义经可移植的 `operations` 引擎(`KernelRead`/`KernelData`/`DirectoryStream`),未复制任何 Linux 门控代码。

## 已实现(读写两路)

**读路径**:`get_security_by_name`(逐组件 LOOKUP 路径解析)、`open`(Open/Opendir,句柄映射 `OpenFile/KernelDirectory::owner_id`)、`close`、`get_file_info`、`read`、`read_directory`(WinFsp 按名续枚举 ↔ LayerFS 按 cookie 续读)、`get_volume_info`(镜像 Linux STATFS 声明)。

**写路径**(经可移植 `operations::mutation` 引擎,Publication 票据语义完整):`create`(文件 0644 / 目录 0755)、`write`(按 `WRITE_WINDOW` 分窗,每窗一次 mutation)、`overwrite`/`set_file_size`(Attributes→size)、`rename`(跨目录带 root 祖先证据)、`set_delete`+`cleanup`(WinFsp 的延迟删除语义,Unlink/Rmdir)、`flush`(no-op,持久性属 commit)。

未实现:符号链接(以普通文件呈现)、reparse point、安全描述符、`set_basic_info`(chmod/utimens)、named streams、`write_to_eof`/`constrained_io`、mmap 写直通。

## 裸跑 runner(已接线)

`core/crates/layerfs-daemon/examples/winfsp_run.rs`:
空目录 init → fork `main` → seal 安装 → `Owner::start_observed`(overlay)→ `open_store` →
`bind`(WorkspaceId×BranchId)→ `prepare_serials` → `kernel_mount`(引擎铸造)→
`WinFspSession::attach` → `std::process::Command` 裸跑(cwd=挂载目录,继承 stdio)→
`commit_captured`(message)→ `detach` + `stop_service`。绕过 daemon 控制协议与 Docker,全进程内直连引擎。

```
cargo run -p layerfs-daemon --example winfsp_run --target x86_64-pc-windows-msvc -- <dir> -- cmd /c dir
```

## 验证

```
cargo check --workspace --exclude layerfs-node --target x86_64-pc-windows-msvc   # 绿
cargo test  -p layerfs-fuse --target x86_64-pc-windows-msvc --lib                # winfsp::tests 通过
```

**端到端已验证**(本机,WinFsp 2.1.25156 驱动安装后):

```
> winfsp_run lfs-run -- cmd /c "echo layerfs-on-windows > hello.txt & dir /b & type hello.txt"
initialized empty store at ...\lfs-run\store.sqlite
mounted at ...\lfs-run\mount
hello.txt                       ← 在 LayerFS 卷内列目录
layerfs-on-windows              ← 在 LayerFS 卷内读回写入内容
committed                       ← commit_captured 入 Store
unmounted

> winfsp_run lfs-run -- cmd /c "type hello.txt & echo second-run >> hello.txt"   ← 第二次运行
hello.txt                       ← 上次 commit 的文件从 Store 历史回来了
layerfs-on-windows
layerfs-on-windows second-run   ← 追加 commit
```

即:init → bind → WinFsp 挂载 → 卷内 create/readdir/read/write → commit 持久化 →
重挂载读回历史,完整闭环在原生 Windows(无 WSL、无 Docker)跑通。

Linux 回归:全部 Rust cfg 检查在本会话中段全绿通过;其后新增的 Unix 分支改动
(os_bytes/os_from_vec、child_path、read_link)与原语义逐行等价。完整 Linux 构建需
Linux C 工具链(bundled SQLite/zstd 的 cc),本 Windows 主机不可用,未重跑。

`layerfs-node`(napi 绑定)沿用上游设计不在 Windows 构建。

## 架构决策:宿主裸跑,无沙箱(2026-10-24 定)

Sandbox 的四个职能中,只有"FUSE 的内核边界"是技术必需,而 WinFsp 挂载天然不需要它。
决策:**Windows 路径不做沙箱**——agent 命令以当前用户身份裸跑在宿主上,
信任模型与裸 git worktree 工作流相同(review 兜底)。LayerFS 的核心价值
(版本化工作区、去重、branch/publish)不依赖沙箱。

### 裸跑接线图(已完成勘察)

| 现有 Linux 路径 | Windows 裸跑对应 |
|---|---|
| `layerfs-sandbox/src/backend/docker/`(客户端,已 cfg(unix)) | 不移植;整个 crate 保持 unix 门控 |
| `layerfs-daemon/src/control/{attach,detach,serving,force}.rs` → `layerfs_fuse::session`(已随 linux 门控) | 写 cfg(windows) 对应物:`WinFspSession::attach/detach` 承担生命周期,`ReadyMount` 应答照常发给控制通道 |
| daemon 的 `workspace exec` = Docker exec 进容器 | `std::process::Command`,cwd = 挂载目录,继承 stdio,退出码即结果 |
| `application/config.rs::secure_process`(非 Linux 拒绝) | Windows 分支:不再拒绝,返回 Ok(裸跑声明) |
| runtime image(Node 24 容器) | 无;命令用宿主环境 |

### 信任模型声明

无沙箱 = 命令可写用户可写的一切;Workspace 之外的写入直接落宿主文件系统,
没有 scratch 语义。等价于今天裸跑 Claude Code / git worktree 的风险水平。

## 到完全体的剩余路线
 
1. **写路径**:`write`/`create`/`rename`/`remove` 接 `operations::mutation`(`KernelMutation`/`Publication` 票据语义),`cleanup` 处理 set_delete。
2. **readdir 效率**:枚举循环里每条目一次 LOOKUP+GETATTR 应改为批量(`KernelDirectoryEntry` 已含 serial/kind,可在 DirectoryStream 层直取)。
3. **daemon 裸跑接线**:按上表——control/attach 的 Windows 对应物 + `std::process::Command` exec 通道 + `secure_process` 放行 Windows。
4. **SDK**:`api/binding`(napi)按需 Windows 化,或沿用 dsh 的 WSL worker。
5. **一致性**:winfsp-tests / pjdfstest 子集跑通后再谈 POSIX 面。

## 许可警示

winfsp-rs(SnowflakePowered)为 **GPLv3**(非 MIT,未见 FLOSS 例外)。LayerFS 是 MIT,与之链接的二进制在分发时需遵守 GPL。本地实验无碍;若要回馈上游,需上游作者决策(换自维护的 winfsp.dll FFI,或取得 winfsp-rs 的例外授权)。
