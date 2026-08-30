local function solve(n, row, cols, diag1, diag2)
  if row == n then return 1 end
  local count = 0
  for c = 0, n - 1 do
    local d1 = row + c
    local d2 = row - c + n
    if cols[c] == 0 and diag1[d1] == 0 and diag2[d2] == 0 then
      cols[c] = 1 diag1[d1] = 1 diag2[d2] = 1
      count = count + solve(n, row + 1, cols, diag1, diag2)
      cols[c] = 0 diag1[d1] = 0 diag2[d2] = 0
    end
  end
  return count
end

local n = tonumber(arg and arg[1]) or 11
local t = os.clock()
local cols, diag1, diag2 = {}, {}, {}
for i = 0, n - 1 do cols[i] = 0 end
for i = 0, 2 * n do diag1[i] = 0 diag2[i] = 0 end
print(string.format("queens(%d) = %d", n, solve(n, 0, cols, diag1, diag2)))
print(string.format("# queens n=%d in %.3fs", n, os.clock() - t))
