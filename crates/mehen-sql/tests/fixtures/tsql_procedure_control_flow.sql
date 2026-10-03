-- sqlfluff:dialect:tsql
create procedure dbo.process_orders @batch int as
begin
  declare @count int = 0;

  if @batch > 0
  begin
    update orders set status = 'WORKING' where batch_id = @batch;
    set @count = @@rowcount;
  end
  else
  begin
    set @count = 0;
  end

  while @count > 0
  begin
    set @count = @count - 1;
    if @count = 5 break;
  end

  begin try
    exec sp_executesql N'drop table scratch';
  end try
  begin catch
    if error_number() = 208 throw;
    return;
  end catch

  return;
end
