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

打开 http://127.0.0.1:8080 查看首页。Ctrl+C 停止服务。`--data-dir` 指定本地业务数据目录，重启时继续使用同一目录。

接口：

- `GET /health` 返回服务状态和产品名称。
- `GET /api/employees` 返回员工列表，首次启动时为空，按 id 升序排列。
- `POST /api/employees` 提交 JSON 对象新建员工档案，成功返回 201 和带服务生成 `id` 的员工对象；字段错误或请求体不是有效 JSON 对象返回 400，工号重复（忽略英文字母大小写）返回 409。
- 未知路径返回 404，已知路径不支持的方法返回 405（员工路径支持 GET、POST）。

新建员工需提供 `employeeNo`（工号）、`name`（姓名）、`department`（部门）、`position`（岗位）、
`effectiveDate`（任职生效日期，YYYY-MM-DD 真实日历日期，可为未来日期），`phone`、`email` 可选。
工号、姓名、部门、岗位去掉首尾空白后不能为空，工号全局唯一（比较时忽略大小写，保留首次填写的写法），
任职状态固定为“在职”。

```sh
curl http://127.0.0.1:8080/health
curl http://127.0.0.1:8080/api/employees
curl -X POST http://127.0.0.1:8080/api/employees \
  -H 'Content-Type: application/json' \
  -d '{"employeeNo":"E001","name":"张三","department":"研发部","position":"工程师","effectiveDate":"2026-10-03"}'
```
