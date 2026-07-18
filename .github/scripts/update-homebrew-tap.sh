#!/usr/bin/env bash
set -euo pipefail

# 用法: update-homebrew-tap.sh <tag> <version>
# 示例: update-homebrew-tap.sh v0.1.0 0.1.0
TAG="$1"
VERSION="$2"
TAP_REPO="gongyichen/homebrew-yi-agent"
REPO="gongyichen/yi-agent"

: "${TAP_REPO_TOKEN:?需要设置 TAP_REPO_TOKEN 环境变量}"

# 从刚上传的 GitHub Release 下载 SHA256 文件
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

echo "Fetching SHA256 files from $BASE_URL..."
INTEL_MAC_SHA=$(curl -sL "$BASE_URL/yi-agent-x86_64-apple-darwin.tar.gz.sha256")
ARM_MAC_SHA=$(curl -sL "$BASE_URL/yi-agent-aarch64-apple-darwin.tar.gz.sha256")
LINUX_SHA=$(curl -sL "$BASE_URL/yi-agent-x86_64-unknown-linux-gnu.tar.gz.sha256")

if [ -z "$INTEL_MAC_SHA" ] || [ -z "$ARM_MAC_SHA" ] || [ -z "$LINUX_SHA" ]; then
  echo "ERROR: 无法获取 SHA256,可能 Release 资产未上传完成"
  exit 1
fi

echo "Intel Mac SHA256: $INTEL_MAC_SHA"
echo "ARM Mac SHA256:   $ARM_MAC_SHA"
echo "Linux SHA256:     $LINUX_SHA"

# 生成新的公式文件
cat > /tmp/yi-agent.rb <<EOF
class YiAgent < Formula
  desc "A coding agent CLI"
  homepage "https://github.com/$REPO"
  version "$VERSION"

  on_macos do
    on_intel do
      url "$BASE_URL/yi-agent-x86_64-apple-darwin.tar.gz"
      sha256 "$INTEL_MAC_SHA"
    end
    on_arm do
      url "$BASE_URL/yi-agent-aarch64-apple-darwin.tar.gz"
      sha256 "$ARM_MAC_SHA"
    end
  end

  on_linux do
    url "$BASE_URL/yi-agent-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "$LINUX_SHA"
  end

  def install
    bin.install "yi-agent"
  end

  test do
    assert_match "yi-agent", shell_output("#{bin}/yi-agent --version")
  end
end
EOF

# Base64 编码文件内容
CONTENT=$(base64 < /tmp/yi-agent.rb | tr -d '\n')

# 获取当前文件 SHA(GitHub Contents API 更新需要)
echo "Fetching current file SHA from $TAP_REPO..."
FILE_SHA=$(curl -sL \
  -H "Authorization: token $TAP_REPO_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$TAP_REPO/contents/Formula/yi-agent.rb" \
  | grep '"sha"' | head -1 | awk -F'"' '{print $4}')

if [ -z "$FILE_SHA" ]; then
  echo "ERROR: 无法获取当前公式文件 SHA,tap 仓库可能未初始化"
  exit 1
fi

echo "Current file SHA: $FILE_SHA"
echo "Updating Formula/yi-agent.rb to version $VERSION..."

# PUT 更新文件
RESPONSE=$(curl -sL \
  -X PUT \
  -H "Authorization: token $TAP_REPO_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$TAP_REPO/contents/Formula/yi-agent.rb" \
  -d "{
    \"message\": \"Update yi-agent to $VERSION\",
    \"content\": \"$CONTENT\",
    \"sha\": \"$FILE_SHA\"
  }")

if echo "$RESPONSE" | grep -q '"content"'; then
  echo "Homebrew tap updated to $VERSION successfully."
else
  echo "ERROR: 更新失败"
  echo "$RESPONSE"
  exit 1
fi
