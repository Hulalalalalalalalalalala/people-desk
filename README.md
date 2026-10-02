# PeopleDesk

员工档案与招聘出勤。

需要Python 3.10 或更新版本，包含标准库 sqlite3。

查看命令帮助：

```sh
python3 app.py --help
```

启动本地服务：

```sh
python3 app.py serve --host 127.0.0.1 --port 8080 --data-dir data
```

打开 http://127.0.0.1:8080 查看首页。首页可以录入新员工档案，提交成功后员工列表中会出现新档案。Ctrl+C 停止服务。`--data-dir` 指定本地业务数据目录，重启时继续使用同一目录，已有员工不会丢失或重新编号。

接口：

- `GET /health` 返回服务状态和产品名称。
- `GET /api/employees` 返回员工列表，按 id 升序排列，外层为 `{"employees": [...]}`。每条记录包含 `id`、`name` 以及工号、部门、岗位、任职生效日期、电话、邮箱、在职状态；旧数据中缺失的档案字段返回 `null`，页面显示为“未填写”。
- `POST /api/employees` 接收 JSON 对象创建员工档案，成功返回 `201` 和保存后的员工对象（含服务生成的 `id`）。字段：
  - `employee_no`（工号，必填）、`name`（姓名，必填）、`department`（部门，必填）、`position`（岗位，必填）：均为字符串，去掉首尾空白后必须有内容；工号在全部员工中唯一，比较时忽略英文字母大小写，展示保留首次填写去掉首尾空白后的写法。
  - `effective_date`（任职生效日期，必填）：`YYYY-MM-DD` 形式的真实日历日期，允许未来日期。
  - `phone`（电话，选填）、`email`（邮箱，选填）：字符串，留空也可保存，不做格式校验。
  - 新建员工的任职状态固定为“在职”。
  - 请求体不是有效 JSON 对象返回 `400`；字段缺失、类型不符或日期无效返回 `400`，错误响应包含中文原因和 `field` 字段；工号重复返回 `409`。整次提交要么全部保存，要么全部失败，不会留下部分档案，也不会覆盖已有员工。
- 未知路径返回 404，已知路径不支持的方法返回 405（`/api/employees` 的 `Allow` 头为 `GET, POST`，其余已知路径仅支持 `GET`）。

```sh
curl http://127.0.0.1:8080/health
curl http://127.0.0.1:8080/api/employees
curl -X POST http://127.0.0.1:8080/api/employees \
  -H "Content-Type: application/json" \
  -d '{"employee_no":"A001","name":"张三","department":"研发部","position":"工程师","effective_date":"2026-10-02","phone":"","email":""}'
```
