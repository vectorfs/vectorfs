# vectorfs

vectorfs 的领域语言。产品两层定位：底层是 S3 Vectors 兼容的向量存储协议，高层是文件即向量化——文件放进来，语义检索出结果。

## Language

### 两层定位

**s3vectors API**:
底层 API：AWS S3 Vectors 协议的兼容面，21 个操作，错误形状与 AWS 一致，标准 AWS SDK 零改动直连。
_Avoid_: 协议面 API、AWS 面

**vectorfs API**:
产品原生 API：把「文件即向量化」暴露为 8 个操作（PutSource、GetSource、GetSourceData、ListSources、DeleteSource、GetIngestStatus、Search、Reindex），wire 上位于 `/x-vf/<Op>` 独立命名空间。
_Avoid_: vf API、文件层 API、superset operations、vectorized API、file2vec

**协议面 (protocol face)**:
s3vectors API 这一层的角色称呼——AWS 客户端看到的那张脸。说层名用 s3vectors API，说角色用协议面。

**文件层 (file layer)**:
实现 vectorfs API 的子系统：把源桶里的文件变成可检索、可回源的向量（扫描、管线、对账、GC）。
_Avoid_: 文件管线（pipeline 是更窄的概念）

**文件即向量化**:
产品口号，描述 vectorfs API 与文件层共同提供的能力；只对外讲，不作 API 名或模块名。
_Avoid_: 文件即向量化 API

### 桶模型

**vector bucket**:
逻辑向量桶 `X`，物理上映射为一对桶。
_Avoid_: 逻辑桶

**source bucket**:
源桶 `X`：纯用户命名空间，普通对象，零系统对象；任何标准工具放入的文件都会被文件层接走。
_Avoid_: 数据桶、用户桶

**system bucket**:
系统桶 `X-vf`：文件层系统数据的存放地——写者锁、摄取状态与派生索引都在这里；删除向量桶时它随之消失，源桶永不。
_Avoid_: 元数据桶

**writer lock**:
桶粒度的单写者锁：一个 vector bucket 同一时刻只有一个文件层写者。
_Avoid_: 全局锁

### 文件层

**source**:
文件层的摄取与溯源单元：源桶里的一个对象。
_Avoid_: document、文档

**pipeline**:
把 source 加工成 chunk 的配置链：normalize → extract（有序回退链）→ chunk → embed（fan-out）。
_Avoid_: 文件层

**chunk**:
source 被切块后的检索单元，一条 chunk 一个向量，携带溯源元数据（`x-vf-src-*`）回指 source。
_Avoid_: 分片、片段

**reindex**:
管线变更后的显式全量重嵌。
_Avoid_: 重建索引

**preset**:
init 时的配置预设：`dev`（仅协议）、`text-rag`、`air-gapped`。
_Avoid_: mode、模式

### 后端

**file backend**:
存储落在本地目录的后端，data dir 即桶根。
_Avoid_: 文件层（同词根，两概念）

**memory backend**:
易失后端，供 CI 与模拟器使用。

**s3 backend**:
落在任意 S3 兼容端点的后端（endpoint 形态，桶来自桶模型）。
