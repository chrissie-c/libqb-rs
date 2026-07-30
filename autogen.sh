#!/bin/sh
# Bootstrap the autotools build system.
# Run this once after cloning or modifying configure.ac / Makefile.am.
set -e
echo "Running autoreconf..."
autoreconf --force --install --verbose
echo ""
echo "Done. Now run:"
echo "  ./configure"
echo "  make"
echo "  make test"
