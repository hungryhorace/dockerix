# Recipes of your own, if a justfile.local exists beside this one (ignored
# by git: list it in .git/info/exclude).
import? 'justfile.local'

# Build the dockerix image from this checkout (fetches and compiles Arculator
# at the pinned commit); tagged with a hash of its sources.
build:
    cargo run -q --release --manifest-path cli/Cargo.toml -- build

# Install the host-side `dockerix` CLI (~/.cargo/bin/dockerix).
install:
    cargo install --locked --path cli

# Start the emulator container in the background and wait for RISC iX to boot.
start:
    cargo run -q --release --manifest-path cli/Cargo.toml -- start

# Halt the guest cleanly and remove the container.
stop:
    cargo run -q --release --manifest-path cli/Cargo.toml -- stop

# Is it running, is it booted.
status:
    cargo run -q --release --manifest-path cli/Cargo.toml -- status

# Open the noVNC console for the running container.
console:
    open http://localhost:6080/vnc.html?autoconnect=1

# Interactive root shell in the guest.
login:
    docker exec -it dockerix-vm rix

# Follow the container's log.
logs:
    docker logs -f dockerix-vm

# The release tarball for this machine, in dist/ (what the release job runs
# on each agent).
package:
    scripts/package.sh "$(rustc -vV | sed -n 's/^host: //p')"
