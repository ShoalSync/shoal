# The release runner

Releases are built on one runner that runs nothing else (Phase 5c step 5): a GitHub Actions
runner registered to this repository alone, as a user of its own, with the label `release` and
none of the default labels, so no pull-request job (`runs-on: self-hosted`) ever lands on it.
It keeps its own toolchains and caches. The release workflow refuses a tag that is not on `main`
or whose commit did not pass CI, and signs every asset with Sigstore.

## Setting one up

On a Linux machine with `gcc`, `gcc-aarch64-linux-gnu`, `pkg-config`, `unzip`, `git`, `python3`
and internet access. Everything else the release needs (Rust, mdBook, a JDK, the Android SDK and
NDK, cosign) is installed by the workflow into the runner's own home.

```sh
sudo useradd -m -s /bin/bash tidefell-release
sudo -u tidefell-release mkdir -p /home/tidefell-release/runner
version=2.337.0
curl -L -o /tmp/runner.tar.gz "https://github.com/actions/runner/releases/download/v$version/actions-runner-linux-x64-$version.tar.gz"
# From the release notes of that version.
echo "70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613  /tmp/runner.tar.gz" | sha256sum -c
sudo -u tidefell-release tar -xzf /tmp/runner.tar.gz -C /home/tidefell-release/runner
cd /home/tidefell-release/runner
token=$(gh api -X POST repos/Tidefell/tidefell/actions/runners/registration-token --jq .token)
sudo -u tidefell-release ./config.sh --unattended --url https://github.com/Tidefell/tidefell \
  --token "$token" --name "$(hostname)-release" --no-default-labels --labels release --work _work
sudo ./svc.sh install tidefell-release
sudo ./svc.sh start
```

`gh api repos/Tidefell/tidefell/actions/runners` then lists it with the label `release` only.

## What it does not protect against

A machine shared with the pull-request runners, whose user can become root, is no boundary
against a hostile pull request: it can reach the release user's files. The runner keeps a
release's builds and caches apart from everyone else's, which stops accidents and cache
poisoning. A release runner on a machine of its own is the boundary; the threat model lists
this as a gap until then.
