#!/usr/bin/env bash
set -euo pipefail

# 用法: update-homebrew-tap.sh <tag> <version>
# 示例: update-homebrew-tap.sh v0.1.0 0.1.0
TAG="$1"
VERSION="$2"
TAP_REPO="gongyichen/homebrew-yi-agent"
REPO="gongyichen/yi-agent"

: "${TAP_REPO_TOKEN:?需要设置 TAP_REPO_TOKEN 环境变量}"

# 验证参数格式
if ! [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "ERROR: TAG 必须匹配 v0.0.0 格式,实际: $TAG" >&2
  exit 1
fi
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "ERROR: VERSION 必须匹配 0.0.0 格式,实际: $VERSION" >&2
  exit 1
fi

# 使用 mktemp 创建临时文件,带 trap 清理
FORMULA_FILE=$(mktemp /tmp/yi-agent.XXXXXX.rb)
trap 'rm -f "$FORMULA_FILE"' EXIT

# 从刚上传的 GitHub Release 下载 SHA256 文件
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

echo "Fetching SHA256 files from $BASE_URL..."
INTEL_MAC_SHA=$(curl -sfL "$BASE_URL/yi-agent-x86_64-apple-darwin.tar.gz.sha256")
ARM_MAC_SHA=$(curl -sfL "$BASE_URL/yi-agent-aarch64-apple-darwin.tar.gz.sha256")
LINUX_SHA=$(curl -sfL "$BASE_URL/yi-agent-x86_64-unknown-linux-gnu.tar.gz.sha256")

if [ -z "$INTEL_MAC_SHA" ] || [ -z "$ARM_MAC_SHA" ] || [ -z "$LINUX_SHA" ]; then
  echo "ERROR: 无法获取 SHA256,可能 Release 资产未上传完成"
  exit 1
fi

echo "Intel Mac SHA256: $INTEL_MAC_SHA"
echo "ARM Mac SHA256:   $ARM_MAC_SHA"
echo "Linux SHA256:     $LINUX_SHA"

# 生成新的公式文件
cat > "$FORMULA_FILE" <<EOF
class YiAgent < Formula
  desc "A coding agent CLI"
  homepage "https://github.com/$REPO"
  head "https://github.com/$REPO.git", branch: "main"
  license "MIT"
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
    assert_match "yi-agent", shell_output("#{bin}/yi-agent --version", 0)
  end
end
EOF

# Base64 编码文件内容
CONTENT=$(base64 < "$FORMULA_FILE" | tr -d '\n')

# 获取当前文件 SHA(支持首次创建场景)
echo "Checking for existing formula in $TAP_REPO..."
FILE_SHA=""
RESPONSE=$(curl -sfL \
  -H "Authorization: token $TAP_REPO_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$TAP_REPO/contents/Formula/yi-agent.rb" 2>/dev/null) || true

if [ -n "$RESPONSE" ]; then
  FILE_SHA=$(echo "$RESPONSE" | jq -r '.sha // empty')
fi

if [ -n "$FILE_SHA" ]; then
  echo "Current file SHA: $FILE_SHA (updating existing formula)"
  PAYLOAD=$(jq -n \
    --arg message "Update yi-agent to $VERSION" \
    --arg content "$CONTENT" \
    --arg sha "$FILE_SHA" \
    '{message: $message, content: $content, sha: $sha}')
else
  echo "No existing formula found (creating new file for first release)"
  PAYLOAD=$(jq -n \
    --arg message "Add yi-agent $VERSION" \
    --arg content "$CONTENT" \
    '{message: $message, content: $content}')
fi

echo "Updating Formula/yi-agent.rb to version $VERSION..."

RESPONSE=$(curl -sfL \
  -X PUT \
  -H "Authorization: token $TAP_REPO_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$TAP_REPO/contents/Formula/yi-agent.rb" \
  -d "$PAYLOAD")

echo "$RESPONSE" | jq -e '.content.url' > /dev/null && \
  echo "Homebrew tap updated to $VERSION successfully." || {
    echo "ERROR: 更新失败"
    echo "$RESPONSE"
    exit 1
  }
