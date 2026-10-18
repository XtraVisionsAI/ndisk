#!/bin/bash
# 在 ndisk-dev 容器内起 Apache mod_dav（Basic 认证），用 OpenDAL WebDAV 后端复跑 run.sh。
set -u
if ! command -v apache2ctl >/dev/null; then
  apt-get install -y -qq apache2 apache2-utils >/dev/null 2>&1
  a2enmod -q dav dav_fs >/dev/null
  htpasswd -bc /etc/apache2/dav.passwd testkey testsecret >/dev/null 2>&1
fi
# 锁库目录必须对 www-data 可写，否则所有写请求 500。
mkdir -p /var/lock/apache2 && chown www-data:www-data /var/lock/apache2
cat > /etc/apache2/conf-enabled/ndisk-dav.conf <<'CONF'
DavLockDB /var/lock/apache2/davlock
Alias /dav /davdata
<Directory /davdata>
  Dav On
  AuthType Basic
  AuthName "ndisk"
  AuthUserFile /etc/apache2/dav.passwd
  Require valid-user
  Options None
  AllowOverride None
</Directory>
CONF
rm -rf /davdata && mkdir -p /davdata && chown www-data:www-data /davdata && chmod 777 /davdata
apache2ctl -k restart 2>/dev/null || apache2ctl start
for i in $(seq 1 20); do curl -fs -u testkey:testsecret -X PROPFIND -H 'Depth: 0' http://127.0.0.1/dav/ >/dev/null && break; sleep 0.3; done
CFG=/test/cfg-webdav.toml S3=/davdata bash /test/run.sh
