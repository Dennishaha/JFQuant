#!/usr/bin/env python3
"""Verify SQLite WAL mode is working correctly."""

import sqlite3
import sys
from pathlib import Path


def check_wal_mode(db_path: str):
    """Check WAL mode configuration and health."""
    db = Path(db_path)

    if not db.exists():
        print(f"❌ Database does not exist: {db_path}")
        return False

    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()

    # Check journal mode
    cursor.execute("PRAGMA journal_mode;")
    journal_mode = cursor.fetchone()[0]
    print(f"Journal mode: {journal_mode}")

    if journal_mode.lower() != "wal":
        print("❌ WAL mode is not enabled!")
        return False

    # Check WAL files
    shm = db.with_suffix(".sqlite-shm")
    wal = db.with_suffix(".sqlite-wal")

    print("\nWAL files:")
    print(f"  -shm: {'✓' if shm.exists() else '✗'} {shm}")
    print(f"  -wal: {'✓' if wal.exists() else '✗'} {wal}")

    if wal.exists():
        wal_size = wal.stat().st_size
        print(f"  WAL size: {wal_size:,} bytes")
        if wal_size > 10 * 1024 * 1024:  # > 10MB
            print("  ⚠️  WAL is large, consider checkpoint")

    # Check synchronous mode
    cursor.execute("PRAGMA synchronous;")
    sync_mode = cursor.fetchone()[0]
    sync_names = {0: "OFF", 1: "NORMAL", 2: "FULL", 3: "EXTRA"}
    print(f"\nSynchronous mode: {sync_names.get(sync_mode, sync_mode)}")

    # Check foreign keys
    cursor.execute("PRAGMA foreign_keys;")
    fk = cursor.fetchone()[0]
    print(f"Foreign keys: {'ON' if fk else 'OFF'}")

    # Check cache size
    cursor.execute("PRAGMA cache_size;")
    cache = cursor.fetchone()[0]
    cache_mb = abs(cache) / 1024 if cache < 0 else cache * 4 / 1024
    print(f"Cache size: {cache_mb:.1f} MB")

    # Check schema version
    cursor.execute("PRAGMA user_version;")
    version = cursor.fetchone()[0]
    print(f"\nSchema version: {version}")

    # Check integrity
    print("\nRunning integrity check...")
    cursor.execute("PRAGMA integrity_check;")
    integrity = cursor.fetchone()[0]

    if integrity == "ok":
        print("✅ Database integrity: OK")
    else:
        print(f"❌ Database integrity: {integrity}")
        conn.close()
        return False

    # Test concurrent read
    print("\nTesting concurrent read access...")
    conn2 = sqlite3.connect(db_path)
    cursor2 = conn2.cursor()

    try:
        cursor2.execute("SELECT COUNT(*) FROM sqlite_master;")
        count = cursor2.fetchone()[0]
        print(f"✅ Concurrent read successful ({count} schema objects)")
    except sqlite3.Error as e:
        print(f"❌ Concurrent read failed: {e}")
        conn.close()
        conn2.close()
        return False
    finally:
        conn2.close()

    conn.close()
    print("\n✅ All WAL checks passed!")
    return True


if __name__ == "__main__":
    db_path = sys.argv[1] if len(sys.argv) > 1 else ".jfquant/storage/index.sqlite"
    success = check_wal_mode(db_path)
    sys.exit(0 if success else 1)
