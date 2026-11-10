"""Read-only Bluetooth refactor guards. Run from any directory with Python 3."""
from hashlib import sha256
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FROZEN = {
    "native/crates/livi-runtime/src/privileged.rs": "2e04fc49e4d654e5364a5e2f7ff975ccff2da41b18feb301e668cf4d3545d995",
    "native/crates/livi-runtime/src/bluetoothd.rs": "b896010b389df3fa63618008a5d01bc0bdf38b8f746dec685a7c1a14b949134d",
    "native/crates/livi-runtime/src/bt.rs": "2585c3fe9f5c3d8967b2cb984057693cd915558f846ea21c7173876c243eda8f",
    "native/crates/livi-runtime/src/hfp.rs": "b63219f03aac2324a1eb02aba52e0a72dd391e7254a0d9c0fc9a6d5ed1add5e7",
    "native/crates/livi-runtime/src/sco.rs": "7c17610c7ebf69234761ec962bc045aad03cd44d1dfb95a7dfe260d5bd0f8d40",
    "native/crates/livi-link-host/src/bt.rs": "bb9e34898a12c41ba7d59729803ffcb54f23fc6ba02c6f3e46164c9bf62169ea",
    "native/crates/livi-btd/src/linux.rs": "dc0fb6781949084e535e0c1a9d813ae81019d727ed4f23aed4dc5cb740980597",
    "native/crates/livi-btd/src/host.rs": "2b6177512753354a11a2ec603403da5d6d0e8997872833e534648a1e18b08504",
    "native/crates/livi-btd/src/hci.rs": "419c5e2305cf71f14ec8ca806cdbc54ed6f709abfb164f2b79261bda308c7c1c",
}

def main():
    for name, expected in FROZEN.items():
        assert sha256((ROOT / name).read_bytes()).hexdigest() == expected, name
    print(f"PASS: {len(FROZEN)} frozen privilege/transport/HFP files match Phase 0")
    cli = (ROOT / "native/crates/livi-helperd/src/main.rs").read_text()
    cli = cli.replace('#[cfg(target_os = "linux")]\nmod bluetooth_linux;\n', "", 1)
    assert sha256(cli.encode()).hexdigest() == "9b330109dea68c01144e0b9a48d26eb58d04fb3492ef0352d09e2e2e4ab241c5", "CLI changed beyond module declaration"
    print("PASS: CLI is identical except for the Linux module declaration")
    policy = (ROOT / "native/crates/livi-runtime/src/bluetooth/policy.rs").read_text()
    production = policy.split("#[cfg(test)]", 1)[0]
    for forbidden in ("zbus", "tokio", "HelperState", "livi_aa", "iap2_", "std::env", "Instant::now"):
        assert forbidden not in production, f"policy acquired dependency: {forbidden}"
    lifecycle = (ROOT / "native/crates/livi-runtime/src/bluetooth/lifecycle.rs").read_text()
    for forbidden in ("zbus", "bt::", "run_accessory", "livi_aa", "iap2_"):
        assert forbidden not in lifecycle, f"supervisor acquired protocol/backend dependency: {forbidden}"
    print("PASS: policy has no I/O/implicit clock and supervisor has no backend/projection dependency")

if __name__ == "__main__":
    main()
