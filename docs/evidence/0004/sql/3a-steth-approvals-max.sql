SELECT MAX(u256_dec(word(data, 0))) AS largest_approval
FROM logs
WHERE address = X'ae7ab96520de3a18e5e111b5eaab095312d7fe84'
  AND topic0  = X'8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925'
  AND block_number BETWEEN 17031168 AND 17039359;
