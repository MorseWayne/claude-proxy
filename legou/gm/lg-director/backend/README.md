# Operations Backend Deployment（本地/测试环境）

当前后端支持两种本地/测试环境运行方式，共用同一套配置项。

说明：以下流程为**后端单独开发 / 联调**场景。  
**生产/一体化发布请统一按仓库根目录 `README.md` 的 Makefile 流程执行**，避免与根目录生产运维脚本重复维护。

以下命令默认都在 `backend/` 目录下执行。

## 前置要求

- Docker 和 Docker Compose
- Go 1.25
- Python 3（运行配置同步 XLSX→CSV 脚本时需安装 `scripts/requirements.txt`）

安装 Python 脚本依赖：

```bash
python3 -m pip install -r scripts/requirements.txt
```

## 配置准备

在 `backend/` 目录下复制环境变量模板：

```bash
cp .env.example .env
```

默认约定：

- `.env` 里的 `POSTGRES_DSN` 和 `MONGO_DEFAULT_URI` 供宿主机直接启动 Go 服务时使用
- `docker-compose` 在启动 `backend` 容器时会自动覆盖为容器网络地址，不需要改 `.env`

关键变量：

- `POSTGRES_DSN`: PostgreSQL 连接串（平台全局数据：用户、项目）
- `MONGO_DEFAULT_URI`: MongoDB 默认连接串（环境业务数据）
- `JWT_SECRET`: JWT 签名密钥
- `SERVER_PORT`: HTTP 端口
- `SERVER_MODE`: Gin 模式，常用值 `debug` / `release`
- `LOG_PATH`: 日志目录

## 数据库架构

| 数据库 | 用途 | 说明 |
|--------|------|------|
| PostgreSQL | 平台全局数据 | `users` / `projects` 表，嵌套字段使用 JSONB |
| MongoDB | 环境业务数据 | 按 `EnvironmentConfig.Group` 路由到对应实例，库名 `ops_{envID}` |

PostgreSQL 表结构在 `migrations/` 目录中维护，服务启动和 seed 命令会自动执行迁移（`CREATE IF NOT EXISTS`，可重复执行）。

MongoDB 分组路由通过 `config.yaml` 中 `mongo.groups` 配置：

```yaml
mongo:
  default_uri: mongodb://localhost:27017
  groups:
    内网环境: mongodb://internal-mongo:27017
    外网环境: mongodb://external-mongo:27017
```

未配置的分组自动使用 `default_uri`。

## 模式一：只用 Docker 启动中间件，后端用 Go 直接运行

1. 启动 PostgreSQL 和 MongoDB：

```bash
docker compose up -d postgres mongodb
```

2. 初始化测试数据（会自动建表）：

```bash
go run ./cmd/seed
```

3. 启动后端：

```bash
go run ./cmd/server
```

服务地址：

- API: `http://localhost:8080`
- 健康检查: `http://localhost:8080/health`
- PostgreSQL: `localhost:5432`
- MongoDB: `localhost:27017`

## 模式二：后端和中间件都使用 Docker Compose（本地调试）

1. 启动完整环境：

```bash
docker compose up --build -d
```

2. 导入测试数据：

```bash
docker compose exec backend ./seed
```

服务地址：

- API: `http://localhost:8080`
- 健康检查: `http://localhost:8080/health`

## 常用命令

停止服务：

```bash
docker compose down
```

停止服务并删除数据卷（PostgreSQL + MongoDB 数据全部清除）：

```bash
docker compose down -v
```

查看后端日志：

```bash
docker compose logs -f backend
```

## 当前限制

- 游戏服下发仍然是 stub
- SVN 同步仍然是 stub
- 飞书 OAuth 仍然未接入
- Casbin 仍然是简化策略

这套部署方案的目标是支撑本地开发、前后端联调和测试环境验证，不是生产部署方案。
