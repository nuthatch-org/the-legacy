SELECT COUNT(*)          AS withdrawals,
       SUM(amount)       AS gwei_total,
       MIN(block_number) AS first_block,
       MIN("index")      AS first_index
FROM withdrawals
WHERE block_number BETWEEN 17031168 AND 17039359;
