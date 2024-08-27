use anyhow::{bail, Context, Error, Result};
use std::{
    fs::{self, File},
    io::{Read, StdoutLock, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
};

use crate::{
    clear_terminal,
    cmd::CmdRunner,
    collections::hash_set_with_capacity,
    embedded::EMBEDDED_FILES,
    exercise::{Exercise, RunnableExercise},
    info_file::ExerciseInfo,
};

const STATE_FILE_NAME: &str = ".rustlings-state.txt";
const BAD_INDEX_ERR: &str = "The current exercise index is higher than the number of exercises";

#[must_use]
pub enum ExercisesProgress {
    // All exercises are done.
    AllDone,
    // The current exercise failed and is still pending.
    CurrentPending,
    // A new exercise is now pending.
    NewPending,
}

pub enum StateFileStatus {
    Read,
    NotRead,
}

pub struct AppState {
    current_exercise_ind: usize,
    exercises: Vec<Exercise>,
    // Caches the number of done exercises to avoid iterating over all exercises every time.
    n_done: u16,
    final_message: String,
    // Preallocated buffer for reading and writing the state file.
    file_buf: Vec<u8>,
    official_exercises: bool,
    cmd_runner: CmdRunner,
}

impl AppState {
    // Update the app state from the state file.
    fn update_from_file(&mut self) -> StateFileStatus {
        self.file_buf.clear();
        self.n_done = 0;

        if File::open(STATE_FILE_NAME)
            .and_then(|mut file| file.read_to_end(&mut self.file_buf))
            .is_err()
        {
            return StateFileStatus::NotRead;
        }

        // See `Self::write` for more information about the file format.
        let mut lines = self.file_buf.split(|c| *c == b'\n').skip(2);

        let Some(current_exercise_name) = lines.next() else {
            return StateFileStatus::NotRead;
        };

        if current_exercise_name.is_empty() || lines.next().is_none() {
            return StateFileStatus::NotRead;
        }

        let mut done_exercises = hash_set_with_capacity(self.exercises.len());

        for done_exerise_name in lines {
            if done_exerise_name.is_empty() {
                break;
            }
            done_exercises.insert(done_exerise_name);
        }

        for (ind, exercise) in self.exercises.iter_mut().enumerate() {
            if done_exercises.contains(exercise.name.as_bytes()) {
                exercise.done = true;
                self.n_done += 1;
            }

            if exercise.name.as_bytes() == current_exercise_name {
                self.current_exercise_ind = ind;
            }
        }

        StateFileStatus::Read
    }

    pub fn new(
        exercise_infos: Vec<ExerciseInfo>,
        final_message: String,
    ) -> Result<(Self, StateFileStatus)> {
        let cmd_runner = CmdRunner::build()?;

        let exercises = exercise_infos
            .into_iter()
            .map(|exercise_info| {
                // Leaking to be able to borrow in the watch mode `Table`.
                // Leaking is not a problem because the `AppState` instance lives until
                // the end of the program.
                let path = exercise_info.path().leak();
                let name = exercise_info.name.leak();
                let dir = exercise_info.dir.map(|dir| &*dir.leak());
                let hint = exercise_info.hint.leak().trim_ascii();

                Exercise {
                    dir,
                    name,
                    path,
                    test: exercise_info.test,
                    strict_clippy: exercise_info.strict_clippy,
                    hint,
                    // Updated in `Self::update_from_file`.
                    done: false,
                }
            })
            .collect::<Vec<_>>();

        let mut slf = Self {
            current_exercise_ind: 0,
            exercises,
            n_done: 0,
            final_message,
            file_buf: Vec::with_capacity(2048),
            official_exercises: !Path::new("info.toml").exists(),
            cmd_runner,
        };

        let state_file_status = slf.update_from_file();

        Ok((slf, state_file_status))
    }

    #[inline]
    pub fn current_exercise_ind(&self) -> usize {
        self.current_exercise_ind
    }

    #[inline]
    pub fn exercises(&self) -> &[Exercise] {
        &self.exercises
    }

    #[inline]
    pub fn n_done(&self) -> u16 {
        self.n_done
    }

    #[inline]
    pub fn current_exercise(&self) -> &Exercise {
        &self.exercises[self.current_exercise_ind]
    }

    #[inline]
    pub fn cmd_runner(&self) -> &CmdRunner {
        &self.cmd_runner
    }

    // Write the state file.
    // The file's format is very simple:
    // - The first line is a comment.
    // - The second line is an empty line.
    // - The third line is the name of the current exercise. It must end with `\n` even if there
    // are no done exercises.
    // - The fourth line is an empty line.
    // - All remaining lines are the names of done exercises.
    fn write(&mut self) -> Result<()> {
        self.file_buf.clear();

        self.file_buf
            .extend_from_slice(b"DON'T EDIT THIS FILE!\n\n");
        self.file_buf
            .extend_from_slice(self.current_exercise().name.as_bytes());
        self.file_buf.push(b'\n');

        for exercise in &self.exercises {
            if exercise.done {
                self.file_buf.push(b'\n');
                self.file_buf.extend_from_slice(exercise.name.as_bytes());
            }
        }

        fs::write(STATE_FILE_NAME, &self.file_buf)
            .with_context(|| format!("Failed to write the state file {STATE_FILE_NAME}"))?;

        Ok(())
    }

    pub fn set_current_exercise_ind(&mut self, exercise_ind: usize) -> Result<()> {
        if exercise_ind == self.current_exercise_ind {
            return Ok(());
        }

        if exercise_ind >= self.exercises.len() {
            bail!(BAD_INDEX_ERR);
        }

        self.current_exercise_ind = exercise_ind;

        self.write()
    }

    pub fn set_current_exercise_by_name(&mut self, name: &str) -> Result<()> {
        // O(N) is fine since this method is used only once until the program exits.
        // Building a hashmap would have more overhead.
        self.current_exercise_ind = self
            .exercises
            .iter()
            .position(|exercise| exercise.name == name)
            .with_context(|| format!("No exercise found for '{name}'!"))?;

        self.write()
    }

    pub fn set_pending(&mut self, exercise_ind: usize) -> Result<()> {
        let exercise = self
            .exercises
            .get_mut(exercise_ind)
            .context(BAD_INDEX_ERR)?;

        if exercise.done {
            exercise.done = false;
            self.n_done -= 1;
            self.write()?;
        }

        Ok(())
    }

    // Official exercises: Dump the original file from the binary.
    // Third-party exercises: Reset the exercise file with `git stash`.
    fn reset(&self, exercise_ind: usize, path: &str) -> Result<()> {
        if self.official_exercises {
            return EMBEDDED_FILES
                .write_exercise_to_disk(exercise_ind, path)
                .with_context(|| format!("Failed to reset the exercise {path}"));
        }

        let output = Command::new("git")
            .arg("stash")
            .arg("push")
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .with_context(|| format!("Failed to run `git stash push -- {path}`"))?;

        if !output.status.success() {
            bail!(
                "`git stash push -- {path}` didn't run successfully: {}",
                String::from_utf8_lossy(&output.stderr),
            );
        }

        Ok(())
    }

    pub fn reset_current_exercise(&mut self) -> Result<&'static str> {
        self.set_pending(self.current_exercise_ind)?;
        let exercise = self.current_exercise();
        self.reset(self.current_exercise_ind, exercise.path)?;

        Ok(exercise.path)
    }

    // Reset the exercise by index and return its name.
    pub fn reset_exercise_by_ind(&mut self, exercise_ind: usize) -> Result<&'static str> {
        if exercise_ind >= self.exercises.len() {
            bail!(BAD_INDEX_ERR);
        }

        self.set_pending(exercise_ind)?;
        let exercise = &self.exercises[exercise_ind];
        self.reset(exercise_ind, exercise.path)?;

        Ok(exercise.name)
    }

    // Return the index of the next pending exercise or `None` if all exercises are done.
    fn next_pending_exercise_ind(&self) -> Option<usize> {
        if self.current_exercise_ind == self.exercises.len() - 1 {
            // The last exercise is done.
            // Search for exercises not done from the start.
            return self.exercises[..self.current_exercise_ind]
                .iter()
                .position(|exercise| !exercise.done);
        }

        // The done exercise isn't the last one.
        // Search for a pending exercise after the current one and then from the start.
        match self.exercises[self.current_exercise_ind + 1..]
            .iter()
            .position(|exercise| !exercise.done)
        {
            Some(ind) => Some(self.current_exercise_ind + 1 + ind),
            None => self.exercises[..self.current_exercise_ind]
                .iter()
                .position(|exercise| !exercise.done),
        }
    }

    /// Official exercises: Dump the solution file form the binary and return its path.
    /// Third-party exercises: Check if a solution file exists and return its path in that case.
    pub fn current_solution_path(&self) -> Result<Option<String>> {
        if cfg!(debug_assertions) {
            return Ok(None);
        }

        let current_exercise = self.current_exercise();

        if self.official_exercises {
            EMBEDDED_FILES
                .write_solution_to_disk(self.current_exercise_ind, current_exercise.name)
                .map(Some)
        } else {
            let sol_path = current_exercise.sol_path();

            if Path::new(&sol_path).exists() {
                return Ok(Some(sol_path));
            }

            Ok(None)
        }
    }

    /// Mark the current exercise as done and move on to the next pending exercise if one exists.
    /// If all exercises are marked as done, run all of them to make sure that they are actually
    /// done. If an exercise which is marked as done fails, mark it as pending and continue on it.
    pub fn done_current_exercise(&mut self, stdout: &mut StdoutLock) -> Result<ExercisesProgress> {
        let exercise = &mut self.exercises[self.current_exercise_ind];
        if !exercise.done {
            exercise.done = true;
            self.n_done += 1;
        }

        if let Some(ind) = self.next_pending_exercise_ind() {
            self.set_current_exercise_ind(ind)?;
            return Ok(ExercisesProgress::NewPending);
        }

        stdout.write_all(RERUNNING_ALL_EXERCISES_MSG)?;

        let n_exercises = self.exercises.len();

        let pending_exercise_ind = thread::scope(|s| {
            let handles = self
                .exercises
                .iter_mut()
                .map(|exercise| {
                    s.spawn(|| {
                        let success = exercise.run_exercise(None, &self.cmd_runner)?;
                        exercise.done = success;
                        Ok::<_, Error>(success)
                    })
                })
                .collect::<Vec<_>>();

            for (exercise_ind, handle) in handles.into_iter().enumerate() {
                write!(stdout, "\rProgress: {exercise_ind}/{n_exercises}")?;
                stdout.flush()?;

                let success = handle.join().unwrap()?;
                if !success {
                    stdout.write_all(b"\n\n")?;
                    return Ok(Some(exercise_ind));
                }
            }

            Ok::<_, Error>(None)
        })?;

        if let Some(pending_exercise_ind) = pending_exercise_ind {
            self.current_exercise_ind = pending_exercise_ind;
            self.n_done = self
                .exercises
                .iter()
                .filter(|exercise| exercise.done)
                .count() as u16;
            self.write()?;
            return Ok(ExercisesProgress::NewPending);
        }

        // Write that the last exercise is done.
        self.write()?;

        clear_terminal(stdout)?;
        stdout.write_all(FENISH_LINE.as_bytes())?;

        let final_message = self.final_message.trim_ascii();
        if !final_message.is_empty() {
            stdout.write_all(final_message.as_bytes())?;
            stdout.write_all(b"\n")?;
        }

        Ok(ExercisesProgress::AllDone)
    }
}

const RERUNNING_ALL_EXERCISES_MSG: &[u8] = b"
All exercises seem to be done.
Recompiling and running all exercises to make sure that all of them are actually done.
";

const FENISH_LINE: &str = "+----------------------------------------------------+
|          You made it to the Fe-nish line!          |
+--------------------------  ------------------------+
                           \\/\x1b[31m
     ▒▒          ▒▒▒▒▒▒▒▒      ▒▒▒▒▒▒▒▒          ▒▒
   ▒▒▒▒  ▒▒    ▒▒        ▒▒  ▒▒        ▒▒    ▒▒  ▒▒▒▒
   ▒▒▒▒  ▒▒  ▒▒            ▒▒            ▒▒  ▒▒  ▒▒▒▒
 ░░▒▒▒▒░░▒▒  ▒▒            ▒▒            ▒▒  ▒▒░░▒▒▒▒
   ▓▓▓▓▓▓▓▓  ▓▓      ▓▓██  ▓▓  ▓▓██      ▓▓  ▓▓▓▓▓▓▓▓
     ▒▒▒▒    ▒▒      ████  ▒▒  ████      ▒▒░░  ▒▒▒▒
       ▒▒  ▒▒▒▒▒▒        ▒▒▒▒▒▒        ▒▒▒▒▒▒  ▒▒
         ▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▒▒▒▒▒▒▒▒▓▓▓▓▓▓▒▒▒▒▒▒▒▒
           ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
             ▒▒▒▒▒▒▒▒▒▒██▒▒▒▒▒▒██▒▒▒▒▒▒▒▒▒▒
           ▒▒  ▒▒▒▒▒▒▒▒▒▒██████▒▒▒▒▒▒▒▒▒▒  ▒▒
         ▒▒    ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒    ▒▒
       ▒▒    ▒▒    ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒    ▒▒    ▒▒
       ▒▒  ▒▒    ▒▒                  ▒▒    ▒▒  ▒▒
           ▒▒  ▒▒                      ▒▒  ▒▒\x1b[0m

";

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_exercise() -> Exercise {
        Exercise {
            dir: None,
            name: "0",
            path: "exercises/0.rs",
            test: false,
            strict_clippy: false,
            hint: "",
            done: false,
        }
    }

    #[test]
    fn next_pending_exercise() {
        let mut app_state = AppState {
            current_exercise_ind: 0,
            exercises: vec![dummy_exercise(), dummy_exercise(), dummy_exercise()],
            n_done: 0,
            final_message: String::new(),
            file_buf: Vec::new(),
            official_exercises: true,
            cmd_runner: CmdRunner::build().unwrap(),
        };

        let mut assert = |done: [bool; 3], expected: [Option<usize>; 3]| {
            for (exercise, done) in app_state.exercises.iter_mut().zip(done) {
                exercise.done = done;
            }
            for (ind, expected) in expected.into_iter().enumerate() {
                app_state.current_exercise_ind = ind;
                assert_eq!(
                    app_state.next_pending_exercise_ind(),
                    expected,
                    "done={done:?}, ind={ind}",
                );
            }
        };

        assert([true, true, true], [None, None, None]);
        assert([false, false, false], [Some(1), Some(2), Some(0)]);
        assert([false, true, true], [None, Some(0), Some(0)]);
        assert([true, false, true], [Some(1), None, Some(1)]);
        assert([true, true, false], [Some(2), Some(2), None]);
        assert([true, false, false], [Some(1), Some(2), Some(1)]);
        assert([false, true, false], [Some(2), Some(2), Some(0)]);
        assert([false, false, true], [Some(1), Some(0), Some(0)]);
    }
}
