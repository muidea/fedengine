# 测试 TLS 材料

server.crt / server.key 是本仓库原始协议测试专用的公开自签名证书与测试私钥，仅验证 localhost TLS 握手、SCRAM 和查询；不包含生产秘密。不要用于部署。证书有效期到 2036 年，更新时须保留 localhost SAN 并重跑 TLS 测试。
