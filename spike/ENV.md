# M0 Spike 环境清单

- 日期：2026-11-03
- 主机：WSL2 `Linux 6.18.54.1-microsoft-standard-WSL2 x86_64`，Debian GNU/Linux 13 (trixie)，root
- 出网代理：`http://172.27.240.1:10801`（`http_proxy`/`https_proxy` 均已设置；`no_proxy` 含 RFC1918 网段）
- apt 源：机器已配 `mirrors.aliyun.com/debian`（trixie）
- 结论：**四项全部装齐并实测通过，无阻塞项**。所有上游（rustup/static.rust-lang.org、index.crates.io、pypi.org、go.dev/dl）经代理均可直连，未使用任何镜像配置（Go 的 goproxy.cn 为机器预存配置，见下）。

---

## 1. Rust（rustup + stable）

**安装源**：官方 `sh.rustup.rs` + `static.rust-lang.org` 直连，未用 rsproxy.cn 镜像。

**坑与绕法**：
1. `curl … https://sh.rustup.rs | sh` 管道安装在下载 `rustup-init` 时被断流：`curl: (56) OpenSSL SSL_read: … unexpected eof while reading`（代理抖动，HEAD 探测 200 但大文件下载中断）。
   绕法：改为手动 `curl --retry 5 --retry-all-errors -C -` 断点续传下载 `rustup-init`（21,139,856 字节）成功后执行 `./rustup-init -y --profile default --default-toolchain stable`。
2. stable 工具链下载慢，前台 600s 超时后转后台继续执行，最终 exit 0 完成。

**实测版本输出**：

```
$ rustc --version
rustc 1.99.1 (58cf8f9c3 2026-10-13)      # ≥ 1.94（lancedb 0.41 MSRV）✓

$ cargo --version
cargo 1.99.1 (5f94df478 2026-08-27)

$ rustup --version
rustup 1.29.2 (d8c92c676 2026-10-21)

$ rustup show active-toolchain
stable-x86_64-unknown-linux-gnu (default)
```

**PATH**：rustup-init 已自动在 `~/.profile:8` 与 `~/.bashrc:50` 写入 `. "$HOME/.cargo/env"`；另写了 `/etc/profile.d/cargo.sh`（内容 `. /root/.cargo/env`）兜底，登录 shell 均可拿到 cargo。

## 2. cargo registry 连通性

**实测**（直连，未改 `~/.cargo/config.toml`，无任何镜像配置）：

```
$ curl -sI https://index.crates.io/config.json
HTTP/2 200        # sparse index 可达
```

功能级验证：临时空项目 `cargo add serde` + `cargo fetch` 实际拉取成功（serde 1.0.229、syn 3.0.9、proc-macro2 1.0.107 等整条依赖链均从 crates.io 下载，`.crate` 文件落入 `~/.cargo/registry/cache/`），即 `index.crates.io` 与 `static.crates.io` 下载域均直连可用。

**结论：直连，无需 sparse 镜像。**

## 3. Python + boto3

**安装源**：apt（trixie，aliyun 镜像源）；boto3 从 pypi.org 直连（`curl -sI https://pypi.org/simple/` → HTTP/2 200），未配 pip 镜像。

**坑与绕法**：Debian 13 启用 PEP 668，裸 `pip3 install boto3` 被 `externally-managed-environment` 拦截。
绕法：`pip3 install --break-system-packages boto3`（root 专用机，可接受）。

**实测版本输出**：

```
$ python3 --version
Python 3.13.5

$ pip3 --version
pip 25.1.1 from /usr/lib/python3/dist-packages/pip (python 3.13)

$ python3 -c "import boto3; print(boto3.__version__)"
1.43.123
```

`python3-venv`/`python3-pip` 均为 apt 安装（候选 3.13.5-1 / 25.1.1+dfsg-1）。

## 4. Go

**版本决策**：`apt-cache policy golang-go` 候选为 `2:1.24~2`（trixie/main）≥ 1.23 → 按任务规则走 apt，未下载官方 tarball，未写 `/etc/profile.d/go.sh`（apt 装的 `/usr/bin/go` 天然在 PATH）。

**实测版本输出**：

```
$ go version
go1.24.4 linux/amd64        # ≥ 1.23（aws-sdk-go-v2 最新要求）✓

$ go env GOPROXY GOVERSION GOTOOLCHAIN
https://goproxy.cn,direct
go1.24.4
auto
```

**注意**：`GOPROXY=https://goproxy.cn,direct` 来自机器预存配置 `/root/.config/go/env`（本次任务未写入）；即 Go 模块走 goproxy.cn 镜像 + 直连兜底。功能级验证：临时模块 `go get github.com/aws/aws-sdk-go-v2/aws@latest` 成功解析并下载 `aws-sdk-go-v2 v1.47.5`（含 smithy-go v1.28.7）。

## 足迹说明

- 本任务在仓库内只新建了本文件 `spike/ENV.md`（及 `spike/` 目录），未改动其他任何文件，未执行任何 git 写操作。
- 仓库外的临时探测目录（`/root/tmp-setup/`：rustup-init 安装包、cargo/go 连通性探测项目）已清理删除。
