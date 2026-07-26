CREATE TABLE projector_checkpoints (
    projector_name TEXT NOT NULL PRIMARY KEY,
    global_position BIGINT NOT NULL
);
