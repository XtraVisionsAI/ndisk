#!/bin/bash
# 补充端到端测试：非 ASCII 文件名、分片服务端复制、HTTP 代理、缓存策略与 TTL 回收。
set -u
export NDISK_S3_SECRET=testsecret RUST_LOG=info,aws_config=warn,aws_smithy_runtime=warn
BIN=/src/target-linux/debug/ndisk; MNT=/mnt/ndisk; S3=/s3data/test
fail=0; check() { if [ "$1" = "0" ]; then echo "PASS: $2"; else echo "FAIL: $2"; fail=1; fi; }
mount_with() { # $1 cfg $2 cache
  fusermount3 -u $MNT 2>/dev/null; mkdir -p $MNT $2
  $BIN -c $1 --cache-dir $2 mount $MNT > /test/extra-mount.log 2>&1 &
  MPID=$!; for i in $(seq 1 30); do mountpoint -q $MNT && break; sleep 0.2; done
}
wait_obj() { # $1 path $2 size
  for i in $(seq 1 80); do [ -f "$1" ] && [ "$(stat -c %s "$1")" = "$2" ] && return 0; sleep 0.25; done; return 1
}

# ---- 1+2: 非 ASCII 文件名 + 分片复制（阈值调到 8 MiB）----
sed 's/^dir_markers = true/dir_markers = true\ncopy_threshold = 8388608/' /test/cfg.toml > /test/cfg-copy.toml
rm -rf /tmp/xc; mount_with /test/cfg-copy.toml /tmp/xc
echo "你好 世界" > "$MNT/中文 文件+1.txt"; check $? "create file with Chinese/space/plus in name"
wait_obj "$S3/中文 文件+1.txt" $(printf '你好 世界\n' | wc -c); check $? "non-ASCII object uploaded"
mv "$MNT/中文 文件+1.txt" "$MNT/重命名 后.txt"; check $? "rename non-ASCII file (CopySource encoding)"
[ -f "$S3/重命名 后.txt" ] && [ ! -f "$S3/中文 文件+1.txt" ]; check $? "non-ASCII rename reflected in S3"
dd if=/dev/urandom of=/tmp/cp.bin bs=1M count=20 status=none
cp /tmp/cp.bin "$MNT/大 文件.bin"; wait_obj "$S3/大 文件.bin" $(stat -c %s /tmp/cp.bin); check $? "20MiB upload"
mv "$MNT/大 文件.bin" "$MNT/moved.bin"; check $? "rename 20MiB file above copy_threshold"
cmp -s /tmp/cp.bin $S3/moved.bin && [ ! -f "$S3/大 文件.bin" ]; check $? "multipart server-side copy content matches"
fusermount3 -u $MNT; wait $MPID

# ---- 3: HTTP 代理 ----
rm -f /tmp/proxy.log; python3 /test/proxy.py 3128 /tmp/proxy.log & PPID_=$!; sleep 0.5
sed 's/^\[cache\]/[network]\nproxy = "http:\/\/u:p@127.0.0.1:3128"\n\n[cache]/' /test/cfg.toml > /test/cfg-proxy.toml
rm -rf /tmp/xp; $BIN -c /test/cfg-proxy.toml --cache-dir /tmp/xp ls / > /tmp/xp.ls 2>&1
grep -q moved.bin /tmp/xp.ls; check $? "ls via HTTP proxy"
rm -rf /tmp/xp; [ "$($BIN -c /test/cfg-proxy.toml --cache-dir /tmp/xp cat '/重命名 后.txt' 2>/dev/null)" = "你好 世界" ]; check $? "cat via HTTP proxy"
[ "$(grep -c ndisk-s3:7070 /tmp/proxy.log)" -ge 2 ]; check $? "requests observed at proxy ($(wc -l < /tmp/proxy.log) logged)"
kill $PPID_ 2>/dev/null
sed 's/^\[cache\]/[network]\nproxy = "http:\/\/127.0.0.1:3999"\n\n[cache]/' /test/cfg.toml > /test/cfg-deadproxy.toml
rm -rf /tmp/xp; timeout 60 $BIN -c /test/cfg-deadproxy.toml --cache-dir /tmp/xp ls / > /dev/null 2>&1; [ $? -ne 0 ]; check $? "dead proxy makes requests fail (traffic really goes through proxy)"

# ---- 4: 缓存策略与 TTL 回收 ----
C=/tmp/xg; rm -rf $C
$BIN -c /test/cfg.toml --cache-dir $C cache download /moved.bin; check $? "cache download"
$BIN -c /test/cfg.toml --cache-dir $C ls / | grep moved.bin | grep -q cached; check $? "ls shows cached"
$BIN -c /test/cfg.toml --cache-dir $C cache policy / ttl:1 | grep -q "Ttl(1s)"; check $? "set root policy ttl:1"
sleep 2.5
$BIN -c /test/cfg.toml --cache-dir $C cache gc > /tmp/gc.json; grep -q '"ttl_evicted": [1-9]' /tmp/gc.json; check $? "gc evicts expired file ($(tr -d '\n ' < /tmp/gc.json))"
$BIN -c /test/cfg.toml --cache-dir $C ls / | grep moved.bin | grep -q cloud; check $? "ls shows cloud after gc"
$BIN -c /test/cfg.toml --cache-dir $C cache pin /moved.bin && sleep 2.5
$BIN -c /test/cfg.toml --cache-dir $C cache gc > /tmp/gc2.json; grep -q '"ttl_evicted": 0' /tmp/gc2.json; check $? "pinned file survives gc"
$BIN -c /test/cfg.toml --cache-dir $C ls / | grep moved.bin | grep -q pinned; check $? "ls shows pinned"
exit $fail
