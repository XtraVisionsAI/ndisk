#!/bin/bash
# 在 ndisk-dev 容器内执行的端到端测试。后端可换：CFG 指向挂载配置，
# S3 指向后端在容器内可直接查看的数据目录（versitygw 的 posix 目录或 WebDAV 的文档根）。
set -u
export NDISK_SECRET=testsecret RUST_LOG=info,aws_config=warn,aws_smithy_runtime=warn
BIN=/src/target-linux/debug/ndisk
CFG=${CFG:-/test/cfg.toml}
MNT=/mnt/ndisk
CACHE=/tmp/ndisk-cache
S3=${S3:-/s3data/test}
fail=0
check() { if [ "$1" = "0" ]; then echo "PASS: $2"; else echo "FAIL: $2"; fail=1; fi; }

fusermount3 -u $MNT 2>/dev/null; rm -rf $CACHE $MNT; mkdir -p $MNT $CACHE
$BIN -c $CFG --cache-dir $CACHE --data-dir $CACHE/data mount $MNT > /test/mount.log 2>&1 &
MPID=$!
for i in $(seq 1 30); do mountpoint -q $MNT && break; sleep 0.2; done
mountpoint -q $MNT; check $? "mounted"

# 1. 小文件写读
echo "hello ndisk" > $MNT/hello.txt; check $? "write small file"
[ "$(cat $MNT/hello.txt)" = "hello ndisk" ]; check $? "read back small file (dirty path)"
sleep 1.5
[ -f $S3/hello.txt ] && [ "$(cat $S3/hello.txt)" = "hello ndisk" ]; check $? "small file arrived in S3 backend"

# 2. 目录
mkdir $MNT/dir1; check $? "mkdir"
[ -d $S3/dir1 ]; check $? "dir marker in S3"

# 3. 大文件（多分片）
dd if=/dev/urandom of=/tmp/big.bin bs=1M count=20 status=none
cp /tmp/big.bin $MNT/dir1/big.bin; check $? "cp 20MiB file"
for i in $(seq 1 60); do [ -f $S3/dir1/big.bin ] && [ "$(stat -c %s $S3/dir1/big.bin)" = "$(stat -c %s /tmp/big.bin)" ] && break; sleep 0.5; done
cmp -s /tmp/big.bin $S3/dir1/big.bin; check $? "20MiB multipart upload content matches"
cmp -s /tmp/big.bin $MNT/dir1/big.bin; check $? "read back 20MiB via mount"

# 4. 追加与截断
echo "more" >> $MNT/hello.txt; sleep 1.5
[ "$(cat $S3/hello.txt)" = "$(printf 'hello ndisk\nmore')" ]; check $? "append then upload"
truncate -s 5 $MNT/hello.txt; sleep 1.5
[ "$(stat -c %s $S3/hello.txt)" = "5" ]; check $? "truncate then upload"

# 5. 重命名、ls、删除
mv $MNT/dir1/big.bin $MNT/dir1/big2.bin; check $? "rename file"
[ -f $S3/dir1/big2.bin ] && [ ! -f $S3/dir1/big.bin ]; check $? "rename reflected in S3"
mv $MNT/dir1 $MNT/dir2; check $? "rename directory"
[ -f $S3/dir2/big2.bin ] && [ ! -d $S3/dir1 ]; check $? "dir rename reflected in S3"
ls -la $MNT $MNT/dir2 > /test/ls.txt 2>&1; grep -q big2.bin /test/ls.txt; check $? "ls shows renamed file"
rm $MNT/hello.txt; check $? "rm file"
[ ! -f $S3/hello.txt ]; check $? "rm reflected in S3"
rmdir $MNT/dir2 2>/dev/null; [ $? -ne 0 ]; check $? "rmdir non-empty dir refused"
rm $MNT/dir2/big2.bin && rmdir $MNT/dir2; check $? "rm file then rmdir"
[ ! -e $S3/dir2 ]; check $? "dir removed in S3"

# 6. 外部写入的对象可见（缓存 TTL 到期后）
echo "external" > $S3/ext.txt; sleep 1
ls $MNT > /dev/null; [ "$(cat $MNT/ext.txt 2>/dev/null)" = "external" ]; check $? "externally created object visible after dir TTL"

# 6b. 未列举过的嵌套路径直接访问（逐级 lookup 走后端 stat；WebDAV 对目录名会回 301）
mkdir -p $S3/extdir/sub && echo deep > $S3/extdir/sub/f.txt && chmod -R a+rwX $S3/extdir
[ "$(cat $MNT/extdir/sub/f.txt 2>/dev/null)" = "deep" ]; check $? "lookup of not-yet-listed nested path"

# 7. 卸载并用新缓存目录重新挂载读取
fusermount3 -u $MNT; wait $MPID; check $? "unmount clean exit"
rm -rf $CACHE; mkdir -p $CACHE
$BIN -c $CFG --cache-dir $CACHE --data-dir $CACHE/data ls / > /test/ls2.txt 2>&1; grep -q ext.txt /test/ls2.txt; check $? "fresh engine lists remote"
$BIN -c $CFG --cache-dir $CACHE --data-dir $CACHE/data status > /test/status.txt 2>&1; check $? "status"
echo "---- status ----"; cat /test/status.txt
echo "---- mount.log (errors/warnings) ----"; grep -E "WARN|ERROR" /test/mount.log | head -20
exit $fail
