-- Set operations with mixed UNION / UNION ALL / EXCEPT.
SELECT product_id FROM sales_2025
UNION ALL
SELECT product_id FROM sales_2026
UNION
SELECT product_id FROM returns
EXCEPT
SELECT product_id FROM discontinued;
