---
description: "Tabular data processing conventions (polars)"
paths:
  - "**/src/data/**"
  - "**/src/etl/**"
  - "**/*.csv"
  - "**/*.parquet"
---

# Data Processing

- For data processing:
  - **ALWAYS** use `polars` instead of other data frame libraries for tabular data manipulation.
  - If a `polars` dataframe will be printed, **NEVER** simultaneously print the number of entries in the dataframe nor the schema as it is redundant.
  - **NEVER** ingest more than 10 rows of a data frame at a time. Only analyze subsets of data to avoid overloading your memory context.
