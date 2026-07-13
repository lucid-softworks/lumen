use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.borrow_mut().extend_from_slice(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn sql_sqlite_adapter_binds_templates_helpers_and_transactions() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = r#"
      (async () => {
        const fs = require("node:fs"), queryPath = `/tmp/lumen-sql-query-${process.pid}.sql`;
        const sql = Bun.SQL("sqlite://:memory:");
        console.log("shape", sql instanceof Bun.SQL, typeof sql, sql.options.adapter, typeof Bun.postgres, Bun.postgres.options.adapter);
        await sql.unsafe("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active INTEGER)");
        const inserted = await sql`INSERT INTO users ${sql({ id: 1, name: "Alice", active: 1 })}`;
        console.log("insert", inserted.count, inserted.lastInsertRowid);
        await sql`INSERT INTO users ${sql([{ id: 2, name: "Bob", active: 0 }, { id: 3, name: "Cara", active: 1 }])}`;
        const ids = [1, 3];
        const rows = await sql`SELECT id, name FROM ${sql("users")} WHERE id IN ${sql(ids)} ORDER BY id`;
        console.log("rows", JSON.stringify(rows));
        console.log("values", JSON.stringify(await sql`SELECT name FROM users ORDER BY id`.values()));
        fs.writeFileSync(queryPath, "SELECT name FROM users WHERE id = ?");
        console.log("file", JSON.stringify(await sql.file(queryPath, [1])));
        fs.unlinkSync(queryPath);
        await sql`UPDATE users SET ${sql({ name: "Updated" })} WHERE id = ${2}`;
        try { await sql.begin(async tx => { await tx`INSERT INTO users (id, name) VALUES (${4}, ${"Rollback"})`; throw new Error("stop"); }); } catch (_) {}
        console.log("transaction", (await sql`SELECT count(*) AS count FROM users`)[0].count, (await sql`SELECT name FROM users WHERE id = 2`)[0].name);
        const reserved = await sql.reserve();
        console.log("reserve", reserved === sql, typeof reserved.release);
        await sql.close();
        try { await sql`SELECT 1`; } catch (error) { console.log("closed", error.message); }
        const fileSql = Bun.SQL("file::memory:"), mysql = Bun.SQL("mysql2://user:pass@localhost/database");
        console.log("detect", fileSql.options.adapter, mysql.options.adapter, typeof mysql.file);
        await fileSql.close();
      })();
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        [
            "shape true function sqlite function postgres",
            "insert 1 1",
            "rows [{\"id\":1,\"name\":\"Alice\"},{\"id\":3,\"name\":\"Cara\"}]",
            "values [[\"Alice\"],[\"Bob\"],[\"Cara\"]]",
            "file [{\"name\":\"Alice\"}]",
            "transaction 3 Updated",
            "reserve true function",
            "closed SQL client is closed",
            "detect sqlite mysql function",
        ]
    );
}
