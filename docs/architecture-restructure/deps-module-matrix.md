# 模块依赖矩阵（use 语句源；行=引用方 → 列=被引方；格=use 处数/涉及文件数）

| from \ to | agents | channels | cli | config | daemon | hot_switch | ids | lib | main | mcp | memory | migration | providers | registry | signal | storage | str_utils | sys_info | tools | tui | update_state |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **agents** |  —  | 32/14 |  | 28/15 |  | 0/0 | 5/2 |  |  | 1/1 | 0/0 |  | 82/24 | 4/1 |  | 10/6 | 7/4 | 0/0 | 1/1 |  |  |
| **channels** | 8/3 |  —  |  | 6/4 |  |  |  |  |  |  | 0/0 |  | 3/1 |  |  |  |  |  |  |  |  |
| **cli** |  |  |  —  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| **config** | 1/1 |  |  |  —  |  |  |  |  |  |  |  |  | 2/2 |  |  |  |  |  |  |  |  |
| **daemon** | 2/1 | 1/1 |  | 5/1 |  —  | 0/0 |  |  |  |  | 0/0 | 0/0 | 6/1 | 0/0 | 0/0 | 0/0 |  |  | 0/0 |  | 0/0 |
| **hot_switch** |  |  |  |  |  |  —  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| **ids** |  |  |  |  |  |  |  —  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| **lib** |  |  |  |  |  |  |  |  —  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| **main** |  |  |  |  |  |  |  |  |  —  |  |  |  |  |  |  |  |  |  |  |  |  |
| **mcp** | 0/0 |  |  |  |  |  |  |  |  |  —  |  |  | 0/0 |  |  |  |  |  |  |  |  |
| **memory** |  |  |  |  |  |  |  |  |  |  |  —  |  |  |  |  |  |  |  |  |  |  |
| **migration** |  |  |  | 0/0 |  |  | 7/1 |  |  |  |  |  —  |  |  |  |  |  |  |  |  |  |
| **providers** | 1/1 |  |  | 1/1 |  |  |  |  |  |  |  |  |  —  |  |  |  |  |  |  |  |  |
| **registry** |  |  |  | 0/0 |  |  |  |  |  |  |  |  | 16/2 |  —  |  |  |  |  |  |  |  |
| **signal** |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  —  |  |  |  |  |  |  |
| **storage** |  | 5/1 |  |  |  |  | 5/1 |  |  |  |  |  | 2/1 |  |  |  —  |  |  |  |  |  |
| **str_utils** |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  —  |  |  |  |  |
| **sys_info** |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  —  |  |  |  |
| **tools** | 42/19 | 7/3 |  | 1/1 |  | 0/0 | 10/3 |  |  |  | 3/1 |  | 70/30 |  |  | 3/2 | 5/4 |  |  —  |  |  |
| **tui** |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  —  |  |
| **update_state** |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  —  |

## 边清单（降序）

- agents → providers: 82 处 use / 24 文件
- tools → providers: 70 处 use / 30 文件
- tools → agents: 42 处 use / 19 文件
- agents → channels: 32 处 use / 14 文件
- agents → config: 28 处 use / 15 文件
- registry → providers: 16 处 use / 2 文件
- agents → storage: 10 处 use / 6 文件
- tools → ids: 10 处 use / 3 文件
- channels → agents: 8 处 use / 3 文件
- agents → str_utils: 7 处 use / 4 文件
- tools → channels: 7 处 use / 3 文件
- migration → ids: 7 处 use / 1 文件
- channels → config: 6 处 use / 4 文件
- daemon → providers: 6 处 use / 1 文件
- tools → str_utils: 5 处 use / 4 文件
- agents → ids: 5 处 use / 2 文件
- storage → ids: 5 处 use / 1 文件
- storage → channels: 5 处 use / 1 文件
- daemon → config: 5 处 use / 1 文件
- agents → registry: 4 处 use / 1 文件
- tools → storage: 3 处 use / 2 文件
- tools → memory: 3 处 use / 1 文件
- channels → providers: 3 处 use / 1 文件
- config → providers: 2 处 use / 2 文件
- storage → providers: 2 处 use / 1 文件
- daemon → agents: 2 处 use / 1 文件
- tools → config: 1 处 use / 1 文件
- providers → config: 1 处 use / 1 文件
- providers → agents: 1 处 use / 1 文件
- daemon → channels: 1 处 use / 1 文件
- config → agents: 1 处 use / 1 文件
- agents → tools: 1 处 use / 1 文件
- agents → mcp: 1 处 use / 1 文件
- tools → hot_switch: 0 处 use / 0 文件
- registry → config: 0 处 use / 0 文件
- migration → config: 0 处 use / 0 文件
- mcp → providers: 0 处 use / 0 文件
- mcp → agents: 0 处 use / 0 文件
- daemon → update_state: 0 处 use / 0 文件
- daemon → tools: 0 处 use / 0 文件
- daemon → storage: 0 处 use / 0 文件
- daemon → signal: 0 处 use / 0 文件
- daemon → registry: 0 处 use / 0 文件
- daemon → migration: 0 处 use / 0 文件
- daemon → memory: 0 处 use / 0 文件
- daemon → hot_switch: 0 处 use / 0 文件
- channels → memory: 0 处 use / 0 文件
- agents → sys_info: 0 处 use / 0 文件
- agents → memory: 0 处 use / 0 文件
- agents → hot_switch: 0 处 use / 0 文件
