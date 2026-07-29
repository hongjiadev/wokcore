# WokCore releases

Each release publishes 14 friendly payloads for six native targets:

| Target | Friendly payloads |
| --- | --- |
| Windows x64 | `WokCore-v<VERSION>-Windows-x86_64-Portable.zip`, `WokCore-v<VERSION>-Windows-x86_64.msi` |
| Windows ARM64 | `WokCore-v<VERSION>-Windows-arm64-Portable.zip`, `WokCore-v<VERSION>-Windows-arm64.msi` |
| macOS Intel | `WokCore-v<VERSION>-macOS-x86_64.tar.gz`, `WokCore-v<VERSION>-macOS-x86_64.zip` |
| macOS Apple silicon | `WokCore-v<VERSION>-macOS-arm64.tar.gz`, `WokCore-v<VERSION>-macOS-arm64.zip` |
| Linux x64 | `WokCore-v<VERSION>-Linux-x86_64.tar.gz`, `WokCore-v<VERSION>-Linux-x86_64.deb`, `WokCore-v<VERSION>-Linux-x86_64.rpm` |
| Linux ARM64 | `WokCore-v<VERSION>-Linux-arm64.tar.gz`, `WokCore-v<VERSION>-Linux-arm64.deb`, `WokCore-v<VERSION>-Linux-arm64.rpm` |

Replace `<VERSION>` with the release version without the leading `v`.

## Minisign verification

Every release payload, update manifest, and `SHA256SUMS` is signed with
Minisign. Download the asset, its adjacent `.minisig` file, and
`WokCore-Minisign.pub`, then verify the signature:

```bash
minisign -Vm <ASSET> \
  -x <ASSET>.minisig \
  -p WokCore-Minisign.pub
```

After verifying `SHA256SUMS` itself, verify the downloaded payload hashes:

```bash
minisign -Vm SHA256SUMS \
  -x SHA256SUMS.minisig \
  -p WokCore-Minisign.pub
sha256sum -c SHA256SUMS
```

Treat the public key distributed with the bundle as a convenience copy.
Pin the trusted key from this repository or another authenticated channel
before using it as a trust anchor.

## Update manifests

`wokcore-update-v2.json` is the six-target public update contract.
`wokcore-update-v1.json` and the five lowercase target-triple archives exist
only so WokCore 0.1.x can update itself. They are removed at v0.2.0.

## Native signing

Every release asset is signed with Minisign. Windows packages are not
Authenticode-signed and macOS packages are not notarized, so the operating
system may display an origin warning.
