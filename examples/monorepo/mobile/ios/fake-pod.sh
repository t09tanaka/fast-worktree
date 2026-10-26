#!/bin/sh
# Stands in for CocoaPods, offline: `install` writes Pods/ from
# Podfile.lock, with the project's absolute path in a generated file, as
# `pod install` does in its xcconfig files.
case "$1" in
  --version) echo "1.15.2" ;;
  install)
    mkdir -p Pods/Target
    cp Podfile.lock Pods/Manifest.lock
    echo "PODS_ROOT = $(pwd -P)/Pods" > Pods/Target/Target.xcconfig
    ;;
  *) echo "usage: fake-pod.sh --version | install" >&2; exit 1 ;;
esac
