#!/bin/sh
# Load the locally built module on the running kernel.
set -eu
cd "$(dirname "$0")/.."

sudo modprobe snd-pcm
sudo insmod kernel/snd-fireface-uc.ko
echo '--- dmesg ---'
sudo dmesg | tail -25
echo '--- aplay ---'
aplay -l || true
