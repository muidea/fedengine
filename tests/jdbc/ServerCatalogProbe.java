import java.sql.*;
import java.util.*;

/** Virtual PG server regression. Arguments: JDBC base URL, then catalog/schema pairs. */
public class ServerCatalogProbe {
    static String q(String s) { return "\"" + s.replace("\\", "\\\\").replace("\"", "\\\"") + "\""; }
    static String qi(String s) { return "\"" + s.replace("\"", "\"\"") + "\""; }
    static void require(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
    public static void main(String[] args) throws Exception {
        require(args.length > 1, "supply JDBC base URL and catalog/schema pairs");
        var p = new Properties();
        p.setProperty("user", System.getenv("JDBC_USER"));
        p.setProperty("password", System.getenv("JDBC_PASSWORD"));
        p.setProperty("sslmode", "disable");
        p.setProperty("socketTimeout", "45");
        p.setProperty("ApplicationName", "FedEngine ServerCatalogProbe");
        var expected = new TreeMap<String,List<String>>();
        for (int i=1; i<args.length; i++) {
            String[] pair = args[i].split("/", 2);
            require(pair.length == 2, "expected catalog/schema");
            expected.put(pair[0], List.of(pair[1].split(",")));
        }
        for (var item : expected.entrySet()) {
            String catalog = item.getKey();
            try (var c = DriverManager.getConnection(args[0] + "/" + catalog, p)) {
                var m = c.getMetaData();
                var catalogs = new TreeSet<String>();
                try (var r = m.getCatalogs()) { while (r.next()) catalogs.add(r.getString(1)); }
                require(catalogs.equals(expected.keySet()), "catalog enumeration differs: " + catalogs);
                var schemas = new TreeSet<String>();
                try (var r = m.getSchemas(catalog, null)) { while (r.next()) schemas.add(r.getString("TABLE_SCHEM")); }
                var expectedSchemas = new TreeSet<>(item.getValue());
                expectedSchemas.addAll(List.of("pg_catalog", "information_schema"));
                require(schemas.equals(expectedSchemas), "unexpected schema: " + schemas);
                try (var s = c.createStatement(); var r = s.executeQuery("SELECT current_database()")) {
                    require(r.next() && catalog.equals(r.getString(1)), "wrong current_database");
                }
                for (String sql : List.of(
                        "SELECT * FROM fedengine_metadata.catalog_status",
                        "SELECT * FROM _fedengine.meta_pg_database",
                        "SELECT * FROM " + qi(catalog) + ".fedengine_metadata.catalog_status",
                        "SELECT * FROM " + qi(catalog) + "._fedengine.meta_pg_database")) {
                    boolean rejected = false;
                    try (var s = c.createStatement()) { s.executeQuery(sql).close(); }
                    catch (SQLException e) { rejected = true; }
                    require(rejected, "internal relation exposed");
                }
                for (String schema : item.getValue()) {
                int tables = 0; String sample = null;
                try (var r = m.getTables(catalog, schema, "%", new String[]{"TABLE", "VIEW", "FOREIGN TABLE"})) {
                    while (r.next()) { tables++; if (sample == null) sample = r.getString("TABLE_NAME"); }
                }
                int coreCount;
                try (var s = c.prepareStatement("SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=? AND c.relkind IN ('r','v','m','p','f')")) {
                    s.setString(1, schema);
                    try (var r=s.executeQuery()) { require(r.next(), "missing count"); coreCount=r.getInt(1); }
                }
                require(tables == coreCount, "getTables and pg_class disagree");
                int columns = 0;
                if (sample != null) {
                    try (var r=m.getColumns(catalog,schema,sample,"%")) { while(r.next()) columns++; }
                    require(columns > 0,"sample columns missing");
                    if (catalog.equals("fedengine")) {
                        try (var r=m.getPrimaryKeys(catalog,schema,sample)) { require(!r.next(), "remote key advertised as enforced local key"); }
                        try (var r=m.getIndexInfo(catalog,schema,sample,false,false)) { require(!r.next(), "remote index advertised as local index"); }
                        try (var s=c.createStatement(); var r=s.executeQuery("SELECT "+qi(schema)+"."+qi(sample)+".* FROM "+qi(catalog)+"."+qi(schema)+"."+qi(sample)+" LIMIT 1")) { while(r.next()) {} }
                    }
                    try (var s=c.createStatement()) {
                        s.setQueryTimeout(30);
                        try(var r=s.executeQuery("SELECT * FROM "+qi(schema)+"."+qi(sample)+" LIMIT 1")) { while(r.next()) {} }
                    }
                }
                System.out.println("{\"catalog\":"+q(catalog)+",\"schema\":"+q(schema)+",\"table_count\":"+tables+",\"sample_column_count\":"+columns+",\"internal_relations_rejected\":true,\"standard_catalogs_queryable\":true}");
                }
            }
        }
        for (String name: List.of("fedengine", "postgres", "template0", "template1", "unregistered_catalog")) {
            if (expected.containsKey(name)) continue;
            boolean rejected = false;
            try (var c = DriverManager.getConnection(args[0]+"/"+name,p)) {}
            catch (SQLException e) { require("3D000".equals(e.getSQLState()), "unexpected connection error for "+name); rejected = true; }
            require(rejected, "invented database accepted: "+name);
        }
        System.out.println("{\"virtual_server_contract\":\"passed\"}");
    }
}
