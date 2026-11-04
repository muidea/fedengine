import java.sql.*;
import java.util.*;

/** Default login, schema federation, qualification and identity checks on the deployed server. */
public class VirtualDatabaseProbe {
    static String qi(String s) { return "\""+s.replace("\"","\"\"")+"\""; }
    static void require(boolean ok,String msg) { if(!ok) throw new AssertionError(msg); }
    static String sample(Connection c,String schema) throws Exception {
        try(var s=c.prepareStatement("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=? AND c.relkind='f' ORDER BY c.relname LIMIT 1")) {
            s.setString(1,schema);try(var r=s.executeQuery()) { require(r.next(),"empty business schema");return r.getString(1); }
        }
    }
    public static void main(String[] args) throws Exception {
        var p=new Properties();p.setProperty("user",System.getenv("JDBC_USER"));p.setProperty("password",System.getenv("JDBC_PASSWORD"));p.setProperty("sslmode","disable");p.setProperty("socketTimeout","45");
        try(var c=DriverManager.getConnection(args[0]+"/",p)) {
            try(var s=c.createStatement();var r=s.executeQuery("SELECT current_database(),current_user,current_schema(),current_setting('search_path')")) {
                require(r.next()&&r.getString(1).equals(p.getProperty("user")),"default database mismatch");require(r.getString(2).equals(p.getProperty("user")),"user mismatch");require(r.getString(3).equals("public"),"default schema mismatch");require(r.getString(4).equals("\"$user\", public"),"search_path mismatch");
            }
            String pg=sample(c,"insight_demo"),my=sample(c,"mysql57");
            try(var s=c.createStatement();var r=s.executeQuery("SELECT count(*) FROM (SELECT 1 FROM insight_demo."+qi(pg)+" LIMIT 1) a CROSS JOIN (SELECT 1 FROM mysql57."+qi(my)+" LIMIT 1) b")) {require(r.next(),"cross-source result missing");}
            try(var s=c.createStatement()) {
                s.execute("SET search_path TO missing, insight_demo");
                try(var r=s.executeQuery("SELECT * FROM "+qi(pg)+" LIMIT 1")) {while(r.next()) {}}
                s.execute("RESET search_path");
            }
            try(var s=c.createStatement()) {
                boolean rejected=false;try(var r=s.executeQuery("SELECT * FROM \"PostgreSQL17\".public."+qi(pg)+" LIMIT 1")) {} catch(SQLException e) {require("0A000".equals(e.getSQLState()),"cross-database SQLSTATE mismatch");rejected=true;}require(rejected,"cross-database extension enabled by default");
            }
            try(var s=c.createStatement();var r=s.executeQuery("SELECT n.nspname,c.relname,c.oid FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind='f' ORDER BY n.nspname,c.relname")) {
                while(r.next()) System.out.println(r.getString(1)+"\t"+r.getString(2)+"\t"+r.getLong(3));
            }
        }
        System.out.println("{\"virtual_database_login_and_federation\":\"passed\"}");
    }
}
