import java.sql.*;
import java.util.*;
/** Fixture: pg_baseline metadata.orders=101, alternate.orders=202, quoted table=303;
 * mysql_metadata metadata.orders contains id=21. Uses no credentials in transcripts. */
public class NamespaceProbe {
    static void value(Connection c, String sql, int expected) throws Exception {
        try(var s=c.createStatement();var r=s.executeQuery(sql)) {
            if(!r.next() || r.getInt(1)!=expected) throw new AssertionError(sql+": expected "+expected);
            System.out.println("{\"sql\":"+MetadataProbe.quoted(sql)+",\"value\":"+expected+"}");
        }
    }
    public static void main(String[] args) throws Exception {
        var p=new Properties();
        p.setProperty("user",System.getenv("JDBC_USER"));
        p.setProperty("password",System.getenv("JDBC_PASSWORD"));
        p.setProperty("sslmode","disable");p.setProperty("prepareThreshold","1");
        try(var pg=DriverManager.getConnection(args[0]+"/pg_baseline",p);
            var mysql=DriverManager.getConnection(args[0]+"/mysql_metadata",p)) {
            value(pg,"SELECT id FROM metadata.orders",101);
            value(mysql,"SELECT id FROM metadata.orders",21);
            value(mysql,"SELECT id FROM pg_baseline.metadata.orders",101);
            value(pg,"SELECT id FROM mysql_metadata.metadata.orders",21);
            try(var s=pg.createStatement()) {s.execute("SET search_path TO alternate, metadata");}
            value(pg,"SELECT id FROM orders",202);
            try(var s=pg.createStatement()) {s.execute("SET search_path TO metadata, alternate");}
            value(pg,"SELECT id FROM orders",101);
            value(pg,"SELECT id FROM metadata.\"Order Line\"",303);
            try(var s=pg.createStatement()) {s.execute("SET search_path TO \"MixedCase\", metadata");}
            value(pg,"SELECT id FROM \"Order Line\"",404);
            try(var s=pg.createStatement()) {s.execute("RESET search_path");}
            value(pg,"SELECT id FROM orders",21);
            int viewOid;
            try(var st=pg.createStatement();var r=st.executeQuery("SELECT oid FROM pg_class WHERE relname='order_totals'")) {
                if(!r.next()) throw new AssertionError("missing view");
                viewOid=r.getInt(1);
            }
            p.setProperty("user","limited");
            try(var limited=DriverManager.getConnection(args[0]+"/pg_baseline",p)) {
                value(limited,"SELECT count(*)::INTEGER FROM pg_class WHERE relkind='r' AND relname='orders'",1);
                value(limited,"SELECT count(*)::INTEGER FROM pg_constraint WHERE contype='f'",0);
                value(limited,"SELECT count(*)::INTEGER FROM pg_class WHERE relname='customers'",0);
                value(limited,"SELECT CASE WHEN pg_get_viewdef("+viewOid+", true) IS NULL THEN 1 ELSE 0 END",1);
            }
        }
    }
}
