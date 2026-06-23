-- Analytics model: chained CTEs feeding a windowed, grouped final query.
WITH base AS (
    SELECT order_id, customer_id, amount, created_at
    FROM orders
    WHERE created_at >= '2026-01-01'
),
per_customer AS (
    SELECT customer_id,
           SUM(amount) AS total,
           COUNT(*) AS order_count
    FROM base
    GROUP BY customer_id
),
ranked AS (
    SELECT pc.customer_id,
           pc.total,
           pc.order_count,
           ROW_NUMBER() OVER (ORDER BY pc.total DESC) AS rank_by_total,
           CASE
               WHEN pc.total > 10000 THEN 'whale'
               WHEN pc.total > 1000 THEN 'regular'
               ELSE 'occasional'
           END AS segment
    FROM per_customer pc
)
SELECT r.customer_id, r.total, r.segment, r.rank_by_total
FROM ranked r
LEFT JOIN customers c ON r.customer_id = c.id
WHERE r.rank_by_total <= 100 AND (c.active OR c.total > 5000)
ORDER BY r.rank_by_total;
