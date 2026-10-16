#!/bin/bash
# Linux FUSE 端到端测试驱动：在 Docker 中启动 S3 兼容服务（versitygw）与带 /dev/fuse 的构建容器，
# 编译 ndisk 并执行 run.sh / crash.sh。在 macOS 或 Linux 宿主机上运行。
#
#   scripts/e2e/e2e.sh          # 构建 + 全部测试
#   scripts/e2e/e2e.sh clean    # 删除容器与卷
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
NET=ndisk-net; S3=ndisk-s3; DEV=ndisk-dev; VOL=ndisk-s3-data

if [ "${1:-}" = "clean" ]; then
  docker rm -f $S3 $DEV >/dev/null 2>&1 || true
  docker volume rm -f $VOL >/dev/null 2>&1 || true
  docker network rm $NET >/dev/null 2>&1 || true
  echo cleaned; exit 0
fi

docker network inspect $NET >/dev/null 2>&1 || docker network create $NET >/dev/null
if ! docker ps --format '{{.Names}}' | grep -qx $S3; then
  docker rm -f $S3 >/dev/null 2>&1 || true
  docker run -d --name $S3 --network $NET -v $VOL:/data \
    -e ROOT_ACCESS_KEY=testkey -e ROOT_SECRET_KEY=testsecret versity/versitygw posix /data >/dev/null
fi
if ! docker ps --format '{{.Names}}' | grep -qx $DEV; then
  docker rm -f $DEV >/dev/null 2>&1 || true
  docker run -d --name $DEV --network $NET --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
    -v "$ROOT":/src -v $VOL:/s3data -v "$HOME/.cargo/registry":/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/src/target-linux rust:bookworm sleep infinity >/dev/null
  docker exec $DEV bash -c 'apt-get update -qq >/dev/null && apt-get install -y -qq fuse3 sqlite3 python3 >/dev/null 2>&1'
fi
docker exec $DEV mkdir -p /s3data/test
docker exec $DEV bash -c 'cd /src && cargo build -p ndisk-cli'
docker exec $DEV bash -c 'rm -rf /test && mkdir -p /test && cp /src/scripts/e2e/* /test/'
echo "=== run.sh ==="; docker exec $DEV bash /test/run.sh
echo "=== crash.sh ==="; docker exec $DEV bash /test/crash.sh
echo "=== extra.sh ==="; docker exec $DEV bash /test/extra.sh
