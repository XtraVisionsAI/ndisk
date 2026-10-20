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
rc=$?

# 大文件流式上传：256 MiB 文件上传期间挂载进程的内存峰值（VmHWM）应远小于文件大小。
BIN=/src/target-linux/debug/ndisk; MNT=/mnt/ndisk; CACHE=/tmp/ndisk-cache-big
export NDISK_SECRET=testsecret
check() { if [ "$1" = "0" ]; then echo "PASS: $2"; else echo "FAIL: $2"; rc=1; fi; }
fusermount3 -u $MNT 2>/dev/null; rm -rf $CACHE; mkdir -p $MNT $CACHE
$BIN -c /test/cfg-webdav.toml --cache-dir $CACHE --data-dir $CACHE/data mount $MNT > /test/mount-big.log 2>&1 &
MPID=$!
for i in $(seq 1 30); do mountpoint -q $MNT && break; sleep 0.2; done
dd if=/dev/urandom of=/tmp/huge.bin bs=1M count=256 status=none
cp /tmp/huge.bin $MNT/huge.bin
for i in $(seq 1 240); do
  [ -f /davdata/huge.bin ] && [ "$(stat -c %s /davdata/huge.bin)" = "$(stat -c %s /tmp/huge.bin)" ] && break; sleep 0.5
done
cmp -s /tmp/huge.bin /davdata/huge.bin; check $? "256MiB streamed upload content matches"
hwm=$(awk '/VmHWM/ {print int($2/1024)}' /proc/$MPID/status)
echo "mount process peak RSS: ${hwm} MiB"
[ "$hwm" -lt 160 ]; check $? "upload memory stays bounded (peak ${hwm} MiB < 160 MiB)"
fusermount3 -u $MNT; wait $MPID
rm -f /tmp/huge.bin /davdata/huge.bin
exit $rc
