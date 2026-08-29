-- sqlfluff:dialect:bigquery
declare x int64 default 0;

if x > 0 then
  update t set c = 1 where id = x;
elseif x < -5 then
  set x = 1;
else
  set x = 2;
end if;

while x < 10 do
  set x = x + 1;
  if x = 5 then
    break;
  end if;
end while;

for rec in (select 1 as n) do
  set x = rec.n;
end for;

begin
  execute immediate 'drop table scratch';
exception when error then
  raise using message = 'boom';
end;
