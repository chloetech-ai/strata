# chloedb-java: ChloeDB from Java

The `ai.chloetech.chloedb` package runs a ChloeDB database inside a Java
process (Java 11 and later), the way sqlite-jdbc runs SQLite: the JVM opens
the file through ChloeDB's native library over JNI, with no server. It is a
JDBC driver too, for code and tools that speak JDBC (`jdbc:chloedb:books.chloedb`).

```sh
mvn install          # Rust (stable), a C compiler, a JDK 11+ and Maven; installs ai.chloetech:chloedb:0.3.3 into ~/.m2
```

```java
import ai.chloetech.chloedb.Database;
import java.util.List;
import java.util.Map;

Map<String, Object> schema = Map.of("table", "books", "key", "id", "fields", List.of(
    Map.of("name", "id", "type", "keyword"),
    Map.of("name", "title", "type", "text"),
    Map.of("name", "year", "type", "int")));

try (Database db = Database.create("books.chloedb", schema)) {
    db.insert(List.of(Map.of("id", "b1", "title", "The river and the dragon", "year", 1999)));
    for (Map<String, Object> row : db.search("dragon")) {
        System.out.println(row.get("id") + " " + row.get("title"));
    }
}
```

Through JDBC:

```java
try (Connection c = DriverManager.getConnection("jdbc:chloedb:books.chloedb");
     ResultSet rs = c.createStatement().executeQuery("SELECT id, title FROM books ORDER BY bm25(title, 'dragon') DESC LIMIT 5")) {
    while (rs.next()) System.out.println(rs.getString("id") + " " + rs.getString("title"));
}
```

The guide is [docs/chloedb/java.md](../docs/chloedb/java.md). The jar
(`target/java/chloedb-0.3.3.jar`) carries the native library for the
platform it was built on. `mvn -Dcargo.profile=ci package` builds faster,
without link-time optimisation, when only the tests matter.

| path | what |
|---|---|
| `src/*.rs` | the native library (Rust, JNI) |
| `src/main/java/ai/chloetech/chloedb/` | `Database`, `Table`, `Result`, the options and `ChloeDBException` |
| `src/main/java/ai/chloetech/chloedb/jdbc/` | the JDBC driver |
| `src/test/java/` | JUnit tests, run by `mvn package` |
