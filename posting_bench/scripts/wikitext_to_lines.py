"""Convert WikiText-103 parquet shards to one paragraph per line (skipping
blank lines and '= Heading =' lines), keeping corpus order (articles stay
contiguous, which clusters docIDs like a URL-ordered web index)."""
import sys
import pyarrow.parquet as pq

out_path, *shards = sys.argv[1:]
docs = 0
with open(out_path, "w", encoding="utf-8") as out:
    for shard in shards:
        for batch in pq.ParquetFile(shard).iter_batches(columns=["text"], batch_size=65536):
            for line in batch.column(0).to_pylist():
                s = line.strip()
                if not s or s.startswith("= "):
                    continue
                out.write(s.replace("\n", " ") + "\n")
                docs += 1
print(f"{docs} documents")
