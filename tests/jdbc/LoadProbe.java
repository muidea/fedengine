import java.sql.*;
import java.util.*;
/** Repeated fresh statements; stdout records wall time, latency and SQLSTATE only. */
public class LoadProbe {
    public static void main(String[] args) throws Exception {
        var p=new Properties();p.setProperty("user",System.getenv("JDBC_USER"));
        p.setProperty("password",System.getenv("JDBC_PASSWORD"));p.setProperty("sslmode","disable");
        p.setProperty("prepareThreshold","0");
        try(var c=DriverManager.getConnection(args[0],p)) {
            for(int i=0;i<Integer.parseInt(args[2]);i++) {
                long wall=System.currentTimeMillis(),start=System.nanoTime();String state=null;
                try(var s=c.createStatement();var r=s.executeQuery(args[1])) {while(r.next()) {r.getObject(1);}}
                catch(SQLException e){state=e.getSQLState();}
                System.out.printf(Locale.ROOT,"{\"wall_ms\":%d,\"latency_ms\":%.3f,\"sqlstate\":%s}%n",wall,(System.nanoTime()-start)/1e6,state==null?"null":"\""+state+"\"");
                Thread.sleep(10);
            }
        }
    }
}
