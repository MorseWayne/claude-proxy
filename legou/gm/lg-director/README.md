# Legou Director

乐狗游戏活动运营管理平台（Legou Director）。

## 项目定位

监视器 + 开关 — 不做内容编辑，聚焦 **配置同步、操作执行、状态监控**。

## 核心功能

- **运营总览**：实时掌握环境状态与活动概况。
- **活动管理**：模板上线、线上干预（reload/隐藏/下线）、模板包批量操作。
- **公告管理**：多语言公告编辑、发布与下线。
- **策划配置**：SVN 配置同步、只读浏览、变更 Diff 查看。
- **服务器管理**：服务器列表查看、Conds 参数维护、分组管理。
- **系统管理**：飞书 OAuth 登录、RBAC 权限模型、多环境切换。

## 技术栈

- **前端**：Vue 3 + Vite + Tailwind CSS + Pinia
- **后端**：Go + Gin
- **外部依赖**：Event Proxy、SVN 配置仓库、飞书开放平台

## 文档指引

详见 [docs/](./docs/) 目录：

- [文档索引](./docs/README.md) — 7 篇核心文档的导航入口

## 一体化部署（Docker Compose）

前后端可通过根目录一键启动（开发环境默认包含 PostgreSQL + MongoDB；生产使用外部数据库）：

```bash
cd /path/to/lg-director

# 先补齐 .env（或直接在 compose 命令里设置变量）
cp docker-compose.env.example .env
```

### 1) 开发 / 联调（本地 build）

```bash
# 一般使用 Makefile 快速启动/关闭
make dev-up
```

### 2) 生产 / 交付（纯镜像，不本地构建）

```bash
# 推荐统一用 Makefile 进行生产交付（构建->打tag->推送->启动）
# 详见“首次发布”一节
make deploy-prod
```

说明：`docker-compose.prod.yml` 为纯生产入口，不再托管本地 PostgreSQL/MongoDB 容器。  
`POSTGRES_HOST`、`MONGO_HOST` 必须准确指向外部可访问数据库（因此后端依赖关系不再固定依赖 compose 内部数据库服务）。

### 3) 生产运维（推荐：统一 Makefile）

```bash
make deploy-prod                  # 一键构建+推送+启动（默认 IMAGE_TAG=1.0.0）
make prod-down                    # 停止
make prod-restart                 # 重启
make prod-seed                    # seed
make prod-clean                   # 清理卷
make prod-status                  # 查看状态
make prod-logs service=backend    # 查看日志（backend/frontend）
```

### 4) 首次发布（build -> tag -> push -> 部署）

建议按固定 tag 流程走，避免误用 `latest`：

```bash
cd /path/to/lg-director

# 先准备镜像版本参数
export IMAGE_TAG=20260525
export REGISTRY=registry.example.com
export BACKEND_IMAGE=${REGISTRY}/lg-director-backend:${IMAGE_TAG}
export FRONTEND_IMAGE=${REGISTRY}/lg-director-frontend:${IMAGE_TAG}

# 一条链路：构建 -> 打 tag -> 推送 -> 启动
make deploy-prod

# 发布后状态核查
make prod-status
```

等价的拆分执行：

```bash
export IMAGE_TAG=20260525
export REGISTRY=registry.example.com
export BACKEND_IMAGE=${REGISTRY}/lg-director-backend:${IMAGE_TAG}
export FRONTEND_IMAGE=${REGISTRY}/lg-director-frontend:${IMAGE_TAG}

make prod-push
make prod-up
```

说明：

- Makefile 里的 `dev-build/up` 会按 `backend/Dockerfile` 和 `fronted/Dockerfile` 构建镜像；
- 生产环境启动链路使用 `docker-compose.prod.yml`，服务运行的是构建产物（`server`、`seed`、静态前端资源），不是运行时 `go run` 或挂载源码。

常用运维（Makefile）：

```bash
make dev-down     # 停止开发环境（保留卷）
# 如需重建，可先 make dev-down 再 make dev-up
make prod-down    # 停止生产环境（保留卷）
make prod-clean   # 停止并清理卷
```

```bash
make dev-up       # 重启开发
make prod-restart # 重启生产
make prod-status  # 查看生产状态
make prod-logs service=backend # 查看生产日志（支持 backend/frontend）
make prod-logs    # 查看全部生产服务日志
```

服务端口：

- `http://localhost`：前端
- `http://localhost:8080/health`：后端健康检查

初始化种子（如需测试账号）：

```bash
# 生产环境执行 seed（如需）
make prod-seed
```

如果要接真实 Beagle proxy、SVN、Feishu，请在 `.env` 中补齐 `PROXY_EVENT_URL`、`SVN_*`、`FEISHU_*`。
