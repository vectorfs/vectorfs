# AWS Bedrock Knowledge Bases API 设计语义调研(2026-11-04)

目的:为 vectorfs 附录E 的文件层 HTTP API(PutSource/GetSource/GetSourceData/ListSources/DeleteSource/Search/Reindex/GetIngestStatus)对齐 AWS Bedrock Knowledge Bases(下称 KB)的官方 API 语义。全部论断来自 docs.aws.amazon.com 的 API Reference / User Guide,逐条附 URL。所有字段名为官方 camelCase 原名。

官方 API Reference 结构说明:KB 控制面 op 在 "Agents for Amazon Bedrock"(服务名 bedrock-agent),检索运行面 op 在 "Agents for Amazon Bedrock Runtime"(服务名 bedrock-agent-runtime),页面 URL 前缀分别为 `API_agent_` 与 `API_agent-runtime_`。操作总清单见 API Reference 目录:https://docs.aws.amazon.com/bedrock/latest/APIReference/Welcome.html

---

## 1. KB API 操作全集(按资源分组)

### 1.1 KnowledgeBase CRUD(控制面)

| 操作 | HTTP 路由 | 关键请求字段 | 关键响应字段 |
|---|---|---|---|
| CreateKnowledgeBase | `PUT /knowledgebases/` | `name`(必), `roleArn`(必), `knowledgeBaseConfiguration`(必), `storageConfiguration`(自管 KB 必、managed KB 不需要), `clientToken`, `description`, `tags` | `knowledgeBase{knowledgeBaseId, knowledgeBaseArn, status, createdAt, updatedAt, failureReasons, storageConfiguration, …}` |
| GetKnowledgeBase / ListKnowledgeBases / UpdateKnowledgeBase / DeleteKnowledgeBase | — | List: `maxResults`(1–1000), `nextToken` | `knowledgeBase` / `knowledgeBaseSummaries[]` |

- `knowledgeBaseConfiguration` 内含 `type` 与 `vectorKnowledgeBaseConfiguration{embeddingModelArn, embeddingModelConfiguration{bedrockEmbeddingModelConfiguration{dimensions, embeddingDataType, …}}, supplementalDataStorageConfiguration}`;另有 `kendraKnowledgeBaseConfiguration{kendraIndexArn}`、`managedKnowledgeBaseConfiguration`、`sqlKnowledgeBaseConfiguration` 分支。
- KB `status` 枚举:`CREATING | ACTIVE | DELETING | UPDATING | FAILED | DELETE_UNSUCCESSFUL | UPDATE_UNSUCCESSFUL`;KB 失败可见性字段为 `failureReasons`(字符串数组)。
- 来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_CreateKnowledgeBase.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_KnowledgeBase.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_ListKnowledgeBases.html

### 1.2 DataSource CRUD(控制面)

| 操作 | HTTP 路由 | 关键请求字段 | 关键响应字段 |
|---|---|---|---|
| CreateDataSource | `PUT /knowledgebases/{knowledgeBaseId}/datasources/` | URI `knowledgeBaseId`(必);body:`name`(必), `dataSourceConfiguration`(必), `dataDeletionPolicy`, `description`, `clientToken`, `serverSideEncryptionConfiguration{kmsKeyArn}`, `vectorIngestionConfiguration` | `dataSource{dataSourceId, knowledgeBaseId, name, status, createdAt, updatedAt, failureReasons, dataDeletionPolicy, dataSourceConfiguration, vectorIngestionConfiguration, …}` |
| GetDataSource / ListDataSources / UpdateDataSource / DeleteDataSource | — | List: `maxResults`(1–1000), `nextToken` | `dataSource` / `dataSourceSummaries[]` |

- `dataSourceConfiguration.type` 枚举:`S3 | WEB | CONFLUENCE | SALESFORCE | SHAREPOINT | CUSTOM | REDSHIFT_METADATA | MANAGED_KNOWLEDGE_BASE_CONNECTOR`;S3 分支为 `s3Configuration{bucketArn, bucketOwnerAccountId, inclusionPrefixes[]}`。
- DataSource `status` 枚举:`AVAILABLE | DELETING | DELETE_UNSUCCESSFUL | CREATING | UPDATING | FAILED`。
- 注意:官方明确 **`chunkingConfiguration` 在创建后不可修改**("You can't change the chunkingConfiguration after you create the data source connector")。
- 来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_CreateDataSource.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_DataSource.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_DataSourceConfiguration.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_ListDataSources.html

### 1.3 IngestionJob 四件套(注意是 4 个,含 Stop)

- `StartIngestionJob`:`PUT /knowledgebases/{knowledgeBaseId}/datasources/{dataSourceId}/ingestionjobs/`,body 仅 `description` + `clientToken`(幂等);**响应 HTTP 202**,`{ingestionJob{…}}`。URL: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_StartIngestionJob.html
- `GetIngestionJob`:`GET /knowledgebases/{kbId}/datasources/{dsId}/ingestionjobs/{ingestionJobId}`,URI 三元组定位,无 body;响应 `{ingestionJob{…}}`。URL: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_GetIngestionJob.html
- `ListIngestionJobs`:query `maxResults`(**1–100**,注意上限与其它 List 不同), `nextToken`, body `filters[]`(IngestionJobFilter)、`sortBy`(IngestionJobSortBy)。IngestionJobFilter 目前仅 `attribute=STATUS`、`operator=EQ`、`values[]`。URL: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_ListIngestionJobs.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_IngestionJobFilter.html
- `StopIngestionJob`:`POST .../ingestionjobs/{ingestionJobId}/stop`。URL: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_StopIngestionJob.html

### 1.4 直接文档摄取(第四组,KnowledgeBaseDocuments 族,2025 新增)

`IngestKnowledgeBaseDocuments` / `ListKnowledgeBaseDocuments` / `GetKnowledgeBaseDocuments` / `DeleteKnowledgeBaseDocuments`,路由 `PUT|GET /knowledgebases/{kbId}/datasources/{dsId}/documents`。见 §7。

### 1.5 检索运行面(Runtime)

- `Retrieve`:`POST /knowledgebases/{knowledgeBaseId}/retrieve`(同步)。URL: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_Retrieve.html
- `RetrieveAndGenerate`:`POST /retrieveAndGenerate`(同步);流式变体 `RetrieveAndGenerateStream`。请求:`input{text}`(必)、`retrieveAndGenerateConfiguration{type: KNOWLEDGE_BASE | EXTERNAL_SOURCES, knowledgeBaseConfiguration{knowledgeBaseId, modelArn, retrievalConfiguration{vectorSearchConfiguration{…}}}, externalSourcesConfiguration{modelArn, sources[]{sourceType, byteContent{contentType,data,identifier} | s3Location{uri}}}}`、`sessionId`、`sessionConfiguration`、`userContext`。响应:`output{text}`, `citations[]{generatedResponsePart{text, span{start,end}}, retrievedReferences[]{content, location, metadata}}`, `sessionId`。URL: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_RetrieveAndGenerate.html

---

## 2. Retrieve 的精确形状

请求(POST /knowledgebases/{knowledgeBaseId}/retrieve):

```
{
  "retrievalQuery": {              // 必填;类型 KnowledgeBaseQuery
    "text": "string",              // 0–20000 字符,可选(text/image 二选一)
    "image": { "format": "…", "inlineContent": <blob> },   // 多模态可选
    "type": "TEXT | IMAGE"         // 可选
  },
  "retrievalConfiguration": {      // 可选;KnowledgeBaseRetrievalConfiguration
    "vectorSearchConfiguration": { // KnowledgeBaseVectorSearchConfiguration
      "numberOfResults": 5,        // Integer 1–100,可选
      "filter": { … },             // RetrievalFilter,可选(见 §3)
      "overrideSearchType": "HYBRID | SEMANTIC",   // 可选
      "implicitFilterConfiguration": {…},
      "rerankingConfiguration": {…}
    },
    "managedSearchConfiguration": { "filter", "numberOfResults", "rerankingConfiguration" }
  },
  "nextToken": "…",                // 可选,1–2048
  "guardrailConfiguration": { "guardrailId", "guardrailVersion" },
  "userContext": { "userId": "…" }
}
```

- `overrideSearchType` 位于 `vectorSearchConfiguration` 内,枚举 `HYBRID | SEMANTIC`;官方注:HYBRID 仅在 OpenSearch Serverless(含 filterable text field)时可用,其余 vector store 只有 SEMANTIC。
- `numberOfResults` 默认值:User Guide 明确 "returns up to five results in the response by default"(默认 5,是上限语义,实际可少于此值;hierarchical chunking 时按 child chunk 计,同 parent 会被合并)。
- 响应:`{ retrievalResults: KnowledgeBaseRetrievalResult[], nextToken, guardrailAction: INTERVENED|NONE }`。**注意返回类型名是 `KnowledgeBaseRetrievalResult`**;`RetrievedReference` 是 RetrieveAndGenerate/InvokeAgent 引用(citations)用的另一类型(无 `score`、无 `documentId`)。

`KnowledgeBaseRetrievalResult` 字段(精确可选性):

| 字段 | 类型 | Required |
|---|---|---|
| `content` | RetrievalResultContent | **Yes** |
| `documentId` | String(1–1825;配合 runtime `GetDocumentContent` 回源取整文档) | No |
| `location` | RetrievalResultLocation | No |
| `metadata` | String → JSON value map(key 1–100 字符) | No |
| `score` | Double(相关度) | No |

- `RetrievalResultContent`:`{ text, type: TEXT|IMAGE|ROW|AUDIO|VIDEO, row[]{columnName, columnValue, type}, byteContent(data-URI base64), audio{s3Uri, transcription}, video{s3Uri, summary} }`,全部可选。
- `RetrievalResultLocation`:`type`(必):`S3 | WEB | CONFLUENCE | SALESFORCE | SHAREPOINT | CUSTOM | KENDRA | SQL | ONEDRIVE | GOOGLEDRIVE`;`s3Location{uri}`(S3 分支,仅一个 `uri` 字段)、`webLocation{url}`、`customDocumentLocation{id}` 等分支。

来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_Retrieve.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_KnowledgeBaseQuery.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_KnowledgeBaseVectorSearchConfiguration.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_KnowledgeBaseRetrievalResult.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_RetrievalResultContent.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_RetrievalResultLocation.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_RetrievalResultS3Location.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_RetrievedReference.html 、https://docs.aws.amazon.com/bedrock/latest/userguide/kb-test-config.html

---

## 3. retrievalFilter 语法,与 S3 Vectors QueryVectors filter 的对照

### 3.1 KB RetrievalFilter(结构化 UNION 语法)

官方明确该类型是 **UNION:一次只能指定一个成员**。算子全集(13 个,精确名):

| 算子 | 载体 | 语义 |
|---|---|---|
| `equals` | FilterAttribute{key, value} | 等于 |
| `notEquals` | FilterAttribute | 不等于(**key 不存在也命中**) |
| `greaterThan` / `greaterThanOrEquals` | FilterAttribute | 数值比较 |
| `lessThan` / `lessThanOrEquals` | FilterAttribute | 数值比较 |
| `in` | FilterAttribute(value 为数组) | 值 ∈ 列表 |
| `notIn` | FilterAttribute(value 为数组) | 值 ∉ 列表 |
| `listContains` | FilterAttribute | metadata 值为数组且含 value |
| `startsWith` | FilterAttribute | 前缀匹配(**官方注:目前仅 OpenSearch Serverless 支持**) |
| `stringContains` | FilterAttribute | 子串匹配(string 或数组成员含子串) |
| `andAll` | Array of RetrievalFilter(**最少 2 项**) | AND 组合 |
| `orAll` | Array of RetrievalFilter(最少 2 项) | OR 组合 |

`FilterAttribute{key: String(1–100), value: JSON value}`,key/value 均 Required。

即:任务假设中的 `gt/gte/lt/lte/stringIn` **不存在**——KB 用长名 `greaterThan` 等;对应 `$nin` 的是 `notIn`,另有 `stringContains`(无 `stringIn`)。组合逻辑不是字段内多算子,而是 `andAll/orAll` 递归节点。

来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_RetrievalFilter.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent-runtime_FilterAttribute.html

### 3.2 S3 Vectors QueryVectors 的 filter(MongoDB 风格 $ 语法)

QueryVectors 请求:`{ vectorBucketName+indexName 或 indexArn, queryVector(必, VectorData union), topK(必, Integer ≥1), filter(JSON value, 可选), returnDistance(默认 false), returnMetadata(默认 false), queryMode: CLASSIC|ENHANCED, nextToken }`;响应 `{ vectors[]{key, distance?, metadata?}, nextToken, distanceMetric: euclidean|cosine }`。权限注意:带 filter 或 returnMetadata=true 需额外 `s3vectors:GetVectors`。来源:https://docs.aws.amazon.com/AmazonS3/latest/API/API_S3VectorBuckets_QueryVectors.html

filter 算子全集(User Guide 官方表,12 个):

| 算子 | 输入类型 | 语义 |
|---|---|---|
| `$eq` | String/Number/Boolean | 等于;对数组 metadata 值,任一元素匹配即真 |
| `$ne` | 同上 | 不等于 |
| `$gt` / `$gte` / `$lt` / `$lte` | Number | 数值比较 |
| `$startsWith` | String | 前缀(需 ENHANCED index 或 queryMode=ENHANCED) |
| `$in` / `$nin` | 非空原始值数组 | ∈ / ∉ |
| `$exists` | Boolean | 字段存在性 |
| `$and` / `$or` | 非空 filter 数组 | 逻辑组合 |

省略算子时默认 `$eq`(`{"genre": "documentary"}`);**同一字段可多算子**(`{"price": {"$gte":10, "$lte":50}}`);ENHANCED index 每次查询最多 **100 个 filter constraints**;ENHANCED 为 pre-filtering(先过滤后 ANN,保证 recall),CLASSIC 为边搜边滤(可能少于 topK);filterable 与 non-filterable metadata 分离(后者建 index 时声明、不可转 filterable)。来源:https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-vectors-metadata-filtering.html

### 3.3 结论:两套不是同一语法

- **语法族不同**:KB RetrievalFilter 是"一节点一算子"的结构化 JSON union(`equals/greaterThan/...` 长名 + `andAll/orAll` 组合节点);S3 Vectors 是 Mongo 风格 `{"field": {"$op": value}}`(`$eq/$gt/...` 短名 + `$and/$or` 数组,支持字段内多算子与裸值=隐式 $eq)。
- **语义覆盖大体重叠但各有独占**:语义上等价的映射是 equals↔$eq、notEquals↔$ne、greaterThan(OrEquals)↔$gte($gt)、lessThan(OrEquals)↔$lte($lt)、in↔$in、notIn↔$nin、startsWith↔$startsWith、andAll↔$and、orAll↔$or;**KB 独有** `listContains`、`stringContains`(子串);**S3 Vectors 独有** `$exists`(存在性)。startsWith 在两边都是受限能力(KB 仅 OpenSearch Serverless;S3 需 ENHANCED)。
- 对 vectorfs 的含义:附录E Search 的 filter 若声明"复用 AWS 语法",应**明确锚定 S3 Vectors QueryVectors 的 $ 语法**(与附录A 数据面同构);KB 的 RetrievalFilter 是另一套,若要对齐 KB 语义需做一层翻译($nin↔notIn、无 $exists 等)。

---

## 4. IngestionJob 模型

`IngestionJob` 字段(IngestionJobId/knowledgeBaseId/dataSourceId 必填;`startedAt`/`updatedAt`/`status` 必填;`description`/`failureReasons`/`statistics`/`textReadyAt` 可选;**无 roleArn 字段**——role 在 KB 层):

- `status` 枚举精确值:`STARTING | IN_PROGRESS | COMPLETE | FAILED | STOPPING | STOPPED`(与任务假设一致,已逐字核对)。
- `statistics`(IngestionJobStatistics,Long 型,8 个,**精确名**):
  - `numberOfDocumentsScanned`
  - `numberOfNewDocumentsIndexed`
  - `numberOfModifiedDocumentsIndexed`
  - `numberOfDocumentsDeleted`(注意:**不是** `numberOfDeletedDocumentsIndexed`)
  - `numberOfDocumentsSkipped`
  - `numberOfDocumentsFailed`
  - `numberOfMetadataDocumentsScanned`
  - `numberOfMetadataDocumentsModified`(注意:**没有** `numberOfMetadataDocumentsDeleted` 这个字段)
- 失败可见性:`failureReasons: string[]`(0–2048 项,每条 ≤2048 字符);DataSource/KB 资源本身也有 `failureReasons`。直接摄取路径的文档级失败见 §7 的 `statusReason`。
- 新增 `textReadyAt`:文本抽取完成、可查询的时间点(抽取完成前不返回)。

来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_IngestionJob.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_IngestionJobStatistics.html

---

## 5. DataSource 配置模型(vectorIngestionConfiguration 等)

### 5.1 dataDeletionPolicy

- 枚举 `RETAIN | DELETE`,可选字段。
- `DELETE`:删除 KB 或 DataSource 资源时,同步删除由该数据源转换出的向量数据(**vector store 本体不删**);`RETAIN`:保留这些向量。
- 默认值:API Reference 注明 **managed KB 只支持 DELETE 且 DELETE 是默认**;S3 Vectors 集成页表述为 "two data deletion policies: `Delete` (default) and `Retain`"。
- 来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_CreateDataSource.html 、https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-vectors-bedrock-kb.html

### 5.2 chunkingConfiguration

- `chunkingStrategy` 枚举:`FIXED_SIZE | NONE | HIERARCHICAL | SEMANTIC`(必填)。
- `fixedSizeChunkingConfiguration{maxTokens: Integer 1–8192(必), overlapPercentage: Integer 1–99(必)}`。
- `hierarchicalChunkingConfiguration{levelConfigurations: [{maxTokens}](必), overlapTokens: Integer ≥1(必)}`。**无总 maxTokens 字段**(任务假设里的 maxTokens 只存在于每个 level 配置)。
- `semanticChunkingConfiguration{maxTokens: 1–8192, bufferSize: 0–1, breakpointPercentileThreshold: 50–99}`(均必填)。
- `NONE` 无参数对象。
- 来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_ChunkingConfiguration.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_FixedSizeChunkingConfiguration.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_HierarchicalChunkingConfiguration.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_SemanticChunkingConfiguration.html

### 5.3 parsingConfiguration

- `parsingStrategy` 枚举:`BEDROCK_FOUNDATION_MODEL | BEDROCK_DATA_AUTOMATION | SMART_PARSING | MULTI_MODAL_EMBEDDINGS`(必填;比任务假设多两个新值)。
- `bedrockDataAutomationConfiguration{parsingModality}`;`bedrockFoundationModelConfiguration{modelArn, parsingModality, parsingPrompt{parsingPromptText}}`。
- 来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_ParsingConfiguration.html

### 5.4 vectorIngestionConfiguration(容器)

`{chunkingConfiguration, parsingConfiguration, customTransformationConfiguration{intermediateStorage{s3Location{uri}}, transformations[{stepToApply, transformationFunction{transformationLambdaConfiguration{lambdaArn}}}]}, contextEnrichmentConfiguration{type, bedrockFoundationModelConfiguration{modelArn, enrichmentStrategyConfiguration{method}}}}`。来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_VectorIngestionConfiguration.html(结构亦见 CreateDataSource 全文)。

---

## 6. storageConfiguration:KB 如何指向 S3 Vectors

- `StorageConfiguration.type` 枚举:`OPENSEARCH_SERVERLESS | PINECONE | REDIS_ENTERPRISE_CLOUD | RDS | MONGO_DB_ATLAS | NEPTUNE_ANALYTICS | OPENSEARCH_MANAGED_CLUSTER | S3_VECTORS`。来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_StorageConfiguration.html
- S3 Vectors 分支 `S3VectorsConfiguration` 只有 **3 个字段**(全部可选,二选一定位 index):`vectorBucketArn`(向量桶 ARN)、`indexName`(3–63 字符)、`indexArn`(index ARN)。来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_S3VectorsConfiguration.html
- KB 与 vector index 的关系:控制台两条路——**Quick create**(Bedrock 代建 S3 vector bucket + index 并配好,SSE-S3 默认)或 **选用户预建的 bucket/index**;API 创建则需自备 storageConfiguration。官方最佳实践:每个 KB 独占一个 vector store 以保证数据同步干净。
- S3 Vectors 后端的 KB 限制:仅 SEMANTIC 检索(无 HYBRID);每 vector 自定义 metadata ≤1KB、≤35 个 key;hierarchical chunking 的父子关系作为 non-filterable metadata 存储,token 数过大可能超限;仅浮点向量(无 binary)。
- 来源:https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-vectors-bedrock-kb.html

---

## 7. 同步/异步语义

- **数据源同步(StartIngestionJob)是纯异步**:响应 HTTP **202** + `ingestionJob` 快照;轮询 GetIngestionJob/ListIngestionJobs;可 StopIngestionJob(状态机含 STOPPING/STOPPED)。来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_StartIngestionJob.html
- **存在直接文档摄取 API**(即任务猜测的 IngestKnowledgeBaseDocuments,2025 推出):`PUT /knowledgebases/{kbId}/datasources/{dsId}/documents`,body `{clientToken, documents[1..10]}`;每个 `KnowledgeBaseDocument{content{dataSourceType, s3{s3Location{uri}} | custom{sourceType, customDocumentIdentifier{id} | inlineContent{type, textContent{data} | byteContent{data, mimeType}} | s3Location{uri, bucketOwnerAccountId}}}, metadata{type: INLINE_ATTRIBUTE_METADATA|S3_LOCATION..., inlineAttributes[{key, value{type, stringValue|numberValue|booleanValue|stringListValue}}], accessControlList[{access, name, type}], s3Location{uri}}}`。**单次最多 10 个文档;响应也是 HTTP 202**,返回 `documentDetails[]{dataSourceId, knowledgeBaseId, identifier, status, statusReason, updatedAt}`——按文档粒度的状态轮询(Get/ListKnowledgeBaseDocuments)。
- 文档级 `status` 枚举(12 值):`INDEXED | PARTIALLY_INDEXED | PENDING | FAILED | METADATA_PARTIALLY_INDEXED | METADATA_UPDATE_FAILED | IGNORED | NOT_FOUND | STARTING | IN_PROGRESS | DELETING | DELETE_IN_PROGRESS`;`DeleteKnowledgeBaseDocuments` 同样 202 + documentDetails。
- 直接摄取仅支持 S3 与 CUSTOM 两类 data source;官方警告:直接摄取的结果**不会写回 S3 数据源**,下次 StartIngestionJob 同步可能覆盖;且**不要同时**发 IngestKnowledgeBaseDocuments 与 StartIngestionJob。
- **Retrieve 是同步的**(200 + retrievalResults);RetrieveAndGenerate 同步(200)另有流式 RetrieveAndGenerateStream。
- 来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_IngestKnowledgeBaseDocuments.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_KnowledgeBaseDocumentDetail.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_DeleteKnowledgeBaseDocuments.html 、https://docs.aws.amazon.com/bedrock/latest/userguide/kb-direct-ingestion.html

---

## 8. 分页与错误模型

- 分页统一 `nextToken`(不透明 string)+ `maxResults`,maxResults 合法域:**ListIngestionJobs 1–100**;**ListKnowledgeBases / ListDataSources / ListKnowledgeBaseDocuments 1–1000**。API Reference 未写服务端默认条数;Retrieve 的 numberOfResults 默认 5(User Guide)。S3 Vectors QueryVectors:`topK` 必填(Integer ≥1,无文档化上限),`nextToken` 1–4096。
- 错误模型与 AWS 通用模型一致,分两层:
  - 服务特有错误(每个 op 页面底部列出):`ValidationException`(400,agent 侧带 `fieldList: ValidationExceptionField[]` 指明出错字段)、`AccessDeniedException`(403)、`ResourceNotFoundException`(404)、`ConflictException`(409)、`ThrottlingException`(429)、`InternalServerException`(500)、`ServiceQuotaExceededException`(agent 侧 402 / runtime 侧 400);runtime 另有 `BadGatewayException`(502)、`DependencyFailedException`(424,带 resourceName)。
  - AWS 全局 Common Errors(签名/请求层):`IncompleteSignature`、`ExpiredTokenException`、`RequestEntityTooLargeException`(413)、`RequestTimeoutException`(408) 等标准集。
- 来源:https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_ListIngestionJobs.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_ListKnowledgeBases.html 、https://docs.aws.amazon.com/AmazonS3/latest/API/API_S3VectorBuckets_QueryVectors.html 、https://docs.aws.amazon.com/bedrock/latest/APIReference/CommonErrors.html (错误清单见各 op 页 Errors 节,如 https://docs.aws.amazon.com/bedrock/latest/APIReference/API_agent_CreateDataSource.html )

---

## 附:与 vectorfs 附录E 的对照速记(供设计讨论,非 AWS 论断)

| vectorfs(附录E) | KB 最接近语义 | 差异要点 |
|---|---|---|
| PutSource(字节+元信息头,入队) | IngestKnowledgeBaseDocuments(inlineContent{byteContent{data, mimeType}} + inlineAttributes metadata,202) | KB 一次 ≤10 docs;PutSource 是单文件原始 body |
| GetSource(状态机+错误) | KnowledgeBaseDocumentDetail{status, statusReason}(12 态枚举) | KB 无 dead-letter 概念 |
| GetSourceData(回源字节) | runtime `GetDocumentContent`(配 retrievalResults[].documentId) | |
| ListSources | ListKnowledgeBaseDocuments(maxResults 1–1000, nextToken) | |
| DeleteSource(删+GC) | DeleteKnowledgeBaseDocuments(202,状态 DELETING→DELETE_IN_PROGRESS);资源级的 dataDeletionPolicy RETAIN/DELETE | |
| Search | Retrieve(numberOfResults 1–100 默认 5、filter、HYBRID/SEMANTIC、score/metadata/location) | vectorfs filter 应锚 S3 Vectors $ 语法(§3.3) |
| Reindex | 无直接对应(重嵌=改 KB embedding 后重建;chunkingConfiguration 创建后不可改) | |
| GetIngestStatus | IngestionJob.statistics 8 计数器 + status 六态 | |
