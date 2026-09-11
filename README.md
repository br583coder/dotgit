to get started clone this repo with

git clone https://github.com/br583coder/dotgit.git

then do 

cargo build 

and

cargo install --path ~/dotgit or wherever the git clone was

then make sure to check with 

dotgit --version

heres how to use it 

first clone a repo with git

then if you wanna upload files to the git repo make sure to do 

dotgit upload ~/path/to/folder/orfile

and when you want to commit the changes do 

dotgit commit 

there will be a prompt for the commit message 

and you dont need to give dotgit your password it integrates with gh auth login or glab auth login
