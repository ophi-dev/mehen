-- Correlated subquery + scalar subquery + EXISTS: high comprehension burden.
SELECT e.employee_id,
       e.name,
       e.salary,
       (SELECT AVG(e2.salary)
        FROM employees e2
        WHERE e2.department_id = e.department_id) AS dept_avg_salary
FROM employees e
WHERE e.salary > (SELECT AVG(e3.salary)
                  FROM employees e3
                  WHERE e3.department_id = e.department_id)
  AND EXISTS (SELECT 1
              FROM projects p
              WHERE p.lead_id = e.employee_id)
  AND e.department_id IN (SELECT d.id FROM departments d WHERE d.active = true);
