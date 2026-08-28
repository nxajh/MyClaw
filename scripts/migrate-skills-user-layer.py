#!/usr/bin/env python3
import os
import sys
import shutil
import argparse

def main():
    parser = argparse.ArgumentParser(description="Migrate skills to user layer")
    parser.add_argument("--rollback", action="store_true", help="Rollback migration")
    args = parser.parse_args()

    base_dir = os.path.expanduser("~/.myclaw")
    if os.environ.get("MYCLAW_BASE_DIR"):
        base_dir = os.environ.get("MYCLAW_BASE_DIR")
    
    # We migrate from base_dir/skills to base_dir/users/01a0151d-997f-7980-9ad1-cd9caf893d87/skills
    target_user_id = "01a0151d-997f-7980-9ad1-cd9caf893d87"
    old_skills_dir = os.path.join(base_dir, "skills")
    new_skills_dir = os.path.join(base_dir, "users", target_user_id, "skills")

    if args.rollback:
        if not os.path.exists(new_skills_dir):
            print("No migrated skills to rollback.")
            return
        os.makedirs(old_skills_dir, exist_ok=True)
        for item in os.listdir(new_skills_dir):
            src = os.path.join(new_skills_dir, item)
            dst = os.path.join(old_skills_dir, item)
            if os.path.exists(dst):
                print(f"Skipping {item} (already exists)")
            else:
                shutil.move(src, dst)
        print("Rollback complete.")
    else:
        if not os.path.exists(old_skills_dir):
            print("No old skills to migrate.")
            return
        os.makedirs(new_skills_dir, exist_ok=True)
        for item in os.listdir(old_skills_dir):
            src = os.path.join(old_skills_dir, item)
            dst = os.path.join(new_skills_dir, item)
            if os.path.exists(dst):
                print(f"Skipping {item} (already exists)")
            else:
                shutil.move(src, dst)
        print("Migration complete.")

if __name__ == "__main__":
    main()
