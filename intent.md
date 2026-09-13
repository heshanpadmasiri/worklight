# Basic principles

- Each process/orchestrator has a unique id. Their corresponding structs should have this so we can do efficient updates/querying
- There shouldn't be separate in memory access and db access. This is stupid
- When creating the panel you shouldn't be dumb and pull only the "active" processes. Then in a seperate thread you should query the database periodically (may be every 5s) and update the state
- Each struct holding states should have reference to the storage in which it was created such that when a mutation is done it can internally update that atomically in the db layer

IMPORTANT: current data representation is crap. Code should be updated to match this not the otherway around

## How states should be held in memory
1. Immutable states
  - These are states that are either set when creating the struct it self (example: process label) or can't change after setting (finishing time <- check the field)
  - Once you have these states you should never query database for these
2. Monotonic states
  - You should keep the terminal state in memory if they have been reached. Else you should always query the database
3. Mutable states
  - You should always query the database for these

- I think there needs be a sync method that updates (only the mutable fields but given you already have the key I don't think there is much overhead in getting all the fields and updating them. This is used for panel)
- Most CLI operations either add whole new data (start) or query and update. Not random data access
  - Thus query should pull in only enough information needed (don't be dumb again think very hard)


## Rendering the panel
- By this nature trying to use these structs them self in the rendering is stupid. Instead each struct must be mapped to a readonly struct that is then used to render the panel (say a snapshot)
- You should build the snapshot using an array of data (process, orch, etc etc)
- When you star the panel query and fill this array with only the unacked processes. Then in the background query all the processes and fill in the array. After that you need to priodically sync the data array
  - Best way to do this is to ask state to sync
- Every time you create a new frame you take a snapshot form each


## States

### ProcessRun
1. `id` this is an auto increment db index. Set when creating the process in `start`
2. `label` immutable set during process start
3. `started_at` immutable set during process start
4. `state` enum (can only go Running -> Finished -> Acknowledged)
  - `Running` (initial state default by start)
  - `Finished` (set by `finish`)
    - `finished_at`
    - `exit_status`
  - `Acknowledged` (set by `Acknowledged` command or by panel)
  NOTE: keep three boolean for Running, Finished or Acknowledged. And seperate columsn for finished_at and exit_status
5. `orchestrator` enum
  - Shell
  - TMUX
  NOTE: in db keep boolean for each orchestrator + foreign key to that db (one column)


### Orchestrator

IMPORTANT: currently all supported orchestrators are immutable

#### Shell
- `id` auto increment
- `cwd` set by start immutable


#### Tmux
- `id` auto increment
- `cwd` set by start immutable
- `pane` pane id set by start immutable

