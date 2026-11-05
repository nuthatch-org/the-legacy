SELECT COUNT(*)                                                         AS transfers,
       SUM(u256_dec(word(data, 0)))                                     AS wei_total,
       SUM(CASE WHEN u256_dec(word(data, 0)) IS NULL THEN 1 ELSE 0 END) AS overflow_rows
FROM logs
WHERE address = X'ae7ab96520de3a18e5e111b5eaab095312d7fe84'
  AND topic0  = X'ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef'
  AND block_number BETWEEN 17031168 AND 17039359;
