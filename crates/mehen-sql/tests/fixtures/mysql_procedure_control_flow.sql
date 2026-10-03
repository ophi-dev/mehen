-- sqlfluff:dialect:mysql
create procedure process_orders(in p_batch int)
begin
  declare v_count int default 0;

  if p_batch > 0 then
    update orders set status = 'WORKING' where batch_id = p_batch;
  elseif p_batch < -5 then
    set v_count = 1;
  else
    set v_count = 2;
  end if;

  while v_count > 0 do
    set v_count = v_count - 1;
  end while;

  repeat
    set v_count = v_count + 1;
  until v_count > 3
  end repeat;

  case v_count
    when 1 then set v_count = 10;
    else set v_count = 20;
  end case;

  prepare stmt from @sql;
  execute stmt;

  signal sqlstate '45000';
end
