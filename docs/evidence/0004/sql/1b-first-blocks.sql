SELECT block_number, COUNT(*) AS withdrawals, SUM(amount) AS gwei
FROM withdrawals
WHERE block_number BETWEEN 17034870 AND 17034880
GROUP BY block_number
ORDER BY block_number;
