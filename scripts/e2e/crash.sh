#!/bin/bash
# 模拟上传中进程崩溃，重挂载后续传。
set -u
export NDISK_S3_SECRET=testsecret RUST_LOG=info,aws_config=warn,aws_smithy_runtime=warn
BIN=/src/target-linux/debug/ndisk; CFG=/test/cfg.toml; MNT=/mnt/ndisk; CACHE=/tmp/ndisk-crash; S3=/s3data/test
fail=0; check() { if [ "$1" = "0" ]; then echo "PASS: $2"; else echo "FAIL: $2"; fail=1; fi; }
fusermount3 -u $MNT 2>/dev/null; rm -rf $CACHE $S3/crash.bin; mkdir -p $MNT $CACHE
# 限速 8 MiB/s，让 20 MiB 上传持续约 2.5 秒以便中途杀死
sed 's/\[cache\]/[network]\nupload_limit = 8388608\n\n[cache]/' $CFG > /test/cfg-slow.toml
$BIN -c /test/cfg-slow.toml --cache-dir $CACHE --data-dir $CACHE/data mount $MNT > /test/crash1.log 2>&1 &
MPID=$!; for i in $(seq 1 30); do mountpoint -q $MNT && break; sleep 0.2; done
dd if=/dev/urandom of=/tmp/crash.bin bs=1M count=20 status=none
cp /tmp/crash.bin $MNT/crash.bin; check $? "cp 20MiB (upload starts in background)"
sleep 1.2
kill -9 $MPID; wait $MPID 2>/dev/null; fusermount3 -u $MNT 2>/dev/null
[ ! -f $S3/crash.bin ]; check $? "object not yet complete in S3 after SIGKILL"
apt-get install -y -qq sqlite3 >/dev/null 2>&1
echo "uploads table after crash: $(sqlite3 $CACHE/meta.sqlite 'select state, length(parts_done) from uploads;')"
PARTS_BEFORE=$(sqlite3 $CACHE/meta.sqlite "select parts_done from uploads;")
# 重新挂载：启动时 resume_uploads
$BIN -c $CFG --cache-dir $CACHE --data-dir $CACHE/data mount $MNT > /test/crash2.log 2>&1 &
MPID=$!; for i in $(seq 1 30); do mountpoint -q $MNT && break; sleep 0.2; done
for i in $(seq 1 60); do [ -f $S3/crash.bin ] && [ "$(stat -c %s $S3/crash.bin)" = "$(stat -c %s /tmp/crash.bin)" ] && break; sleep 0.5; done
cmp -s /tmp/crash.bin $S3/crash.bin; check $? "upload resumed and content matches after remount"
cmp -s /tmp/crash.bin $MNT/crash.bin; check $? "read back via mount"
[ -z "$(sqlite3 $CACHE/meta.sqlite 'select * from uploads;')" ]; check $? "uploads table empty after completion"
echo "parts persisted before crash: $PARTS_BEFORE"
grep -E "upload complete|resume" /test/crash2.log | head -3
fusermount3 -u $MNT; wait $MPID
exit $fail
