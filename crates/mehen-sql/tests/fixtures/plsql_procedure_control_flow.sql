-- sqlfluff:dialect:oracle
create or replace procedure process_orders(p_batch number) is
  v_count pls_integer := 0;
begin
  if p_batch > 0 then
    update orders set status = 'WORKING' where batch_id = p_batch;
    v_count := v_count + 1;
  elsif p_batch < -10 and v_count = 0 then
    raise_application_error(-20001, 'bad batch');
  else
    null;
  end if;

  while v_count > 0 loop
    v_count := v_count - 1;
    exit when v_count = 5;
  end loop;

  for i in 1..3 loop
    v_count := v_count + i;
  end loop;

  execute immediate 'drop table scratch';
  return;
exception
  when no_data_found then
    raise;
  when others then
    raise;
end process_orders;
/
