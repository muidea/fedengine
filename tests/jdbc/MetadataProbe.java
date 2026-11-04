import java.sql.*;
import java.util.*;

/** Real PostgreSQL JDBC metadata calls; stdout is a deterministic, JSON-readable transcript. */
public class MetadataProbe {
    static String quoted(String s) {
        if (s == null) return "null";
        return "\"" + s.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t") + "\"";
    }
    static void result(String name, ResultSet rs) throws Exception {
        try (rs) {
            var md = rs.getMetaData();
            var columns = new ArrayList<String>();
            for (int i=1;i<=md.getColumnCount();i++) columns.add(quoted(md.getColumnLabel(i)));
            System.out.print("{\"method\":"+quoted(name)+",\"columns\":["+String.join(",",columns)+"],\"rows\":[");
            boolean first=true;
            while (rs.next()) {
                if (!first) System.out.print(","); first=false;
                var row = new ArrayList<String>();
                for(int i=1;i<=md.getColumnCount();i++) row.add(quoted(rs.getString(i)));
                System.out.print("["+String.join(",",row)+"]");
            }
            System.out.println("]}");
        }
    }
    public static void main(String[] args) throws Exception {
        var props = new Properties();
        props.setProperty("user",System.getenv().getOrDefault("JDBC_USER","postgres"));
        props.setProperty("password",System.getenv("JDBC_PASSWORD"));
        props.setProperty("sslmode","disable");
        props.setProperty("prepareThreshold","1");
        props.setProperty("ApplicationName","FedEngine MetadataProbe");
        try(var c=DriverManager.getConnection(args[0],props)) {
            c.setAutoCommit(true);
            var m=c.getMetaData();
            String schema=args.length>1?args[1]:"sales";
            result("getCatalogs",m.getCatalogs());
            result("getSchemas",m.getSchemas(c.getCatalog(),schema));
            result("getTables",m.getTables(c.getCatalog(),schema,"%",new String[]{"TABLE","VIEW"}));
            result("getColumns",m.getColumns(c.getCatalog(),schema,"orders","%"));
            result("getPrimaryKeys",m.getPrimaryKeys(c.getCatalog(),schema,"orders"));
            result("getImportedKeys",m.getImportedKeys(c.getCatalog(),schema,"orders"));
            result("getExportedKeys",m.getExportedKeys(c.getCatalog(),schema,"customers"));
            result("getCrossReference",m.getCrossReference(c.getCatalog(),schema,"customers",c.getCatalog(),schema,"orders"));
            result("getIndexInfo",m.getIndexInfo(c.getCatalog(),schema,"orders",false,false));
            result("getTypeInfo",m.getTypeInfo());
        }
    }
}
